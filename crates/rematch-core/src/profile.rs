//! The profile (DESIGN §20): `REMATCH_ROOT/profile/` is a git working tree holding what one
//! person's omp setup is made of, synced across machines by `rematch profile sync`:
//!
//! ```text
//! skills/<name>/              kept entries' dirs (the keep writes here) and adopted hand-written skills
//! agents/<name>.md            kept agents' AGENT.md and adopted hand-written agents
//! fragments/<name>.md         AGENTS.fragment.md of kept entries whose dir is not under skills/
//! hooks/<id>.ts               approved compiled hooks (rematch-hooks/<id>.ts links here)
//! config/policy.json          REMATCH_ROOT/policy.json links here
//! config/roles.yml            logical roles → model selector per machine (`config::resolve_role`)
//! config/domains/<branch>.yml branches/<branch>/domain.yml links here
//! improver/<file>             improver files the user edited (hash ≠ improver/.shipped/<file>)
//! profile.json                {version, machines}
//! ```
//!
//! This module holds the pure parts: layout, the views that link the profile into the places
//! omp and rematch read (`<agent_dir>/skills`, `<agent_dir>/agents`, `rematch-hooks/`,
//! `policy.json`, `branches/*/domain.yml`, `improver/`), the collection of local files into the
//! profile, `roles.yml` and the conflicts file. Git lives in the binary (`profile_cmd`).
//! Per machine and never here: ledger, journal, lake, staging, traces, pending, credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::RematchPaths;

pub const PROFILE_DIR: &str = "profile";
pub const PROFILE_JSON: &str = "profile.json";
/// Paths of the last sync's rebase conflicts, one per line (per machine, gitignored).
pub const CONFLICTS_FILE: &str = ".conflicts";
/// Where each `skills/<name>` came from, as of the last sync (per machine, gitignored).
pub const INDEX_FILE: &str = ".index.json";
pub const ROLES_FILE: &str = "roles.yml";
pub const PROFILE_VERSION: u32 = 1;

/// Lines every profile's `.gitignore` carries: per-machine files inside synced dirs.
pub const GITIGNORE_LINES: &[&str] = &[
    CONFLICTS_FILE,
    INDEX_FILE,
    // The ledger mirror (absolute paths, counters' evidence) and the reflector's notes are
    // written from this machine's ledger.
    "skills/*/rematch.json",
    "skills/*/EXPERIENCE.md",
    // Temporary files of atomic writes (policy.json, links).
    ".*.tmp*",
    "**/.*.tmp*",
];

pub fn dir(paths: &RematchPaths) -> PathBuf {
    paths.root.join(PROFILE_DIR)
}

/// A profile exists once `profile/.git` does (`profile init`).
pub fn present(paths: &RematchPaths) -> bool {
    dir(paths).join(".git").exists()
}

pub fn skills_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("skills")
}

pub fn agents_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("agents")
}

pub fn hooks_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("hooks")
}

pub fn fragments_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("fragments")
}

pub fn config_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("config")
}

pub fn domains_dir(paths: &RematchPaths) -> PathBuf {
    config_dir(paths).join("domains")
}

pub fn improver_dir(paths: &RematchPaths) -> PathBuf {
    dir(paths).join("improver")
}

pub fn policy_file(paths: &RematchPaths) -> PathBuf {
    config_dir(paths).join(crate::domain::POLICY_FILE)
}

pub fn roles_file(paths: &RematchPaths) -> PathBuf {
    config_dir(paths).join(ROLES_FILE)
}

/// Create the layout's directories and make `.gitignore` carry [`GITIGNORE_LINES`] (missing
/// lines appended; the user's own lines stay).
pub fn ensure_layout(paths: &RematchPaths) -> Result<()> {
    for d in [skills_dir(paths), agents_dir(paths), hooks_dir(paths), fragments_dir(paths), domains_dir(paths), improver_dir(paths)] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    let gi = dir(paths).join(".gitignore");
    let current = std::fs::read_to_string(&gi).unwrap_or_default();
    let missing: Vec<&str> = GITIGNORE_LINES.iter().copied().filter(|l| !current.lines().any(|c| c.trim() == *l)).collect();
    if !missing.is_empty() {
        let mut text = current;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("# rematch profile: per-machine files (DESIGN §20)\n");
        }
        for l in missing {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(&gi, text).with_context(|| format!("write {}", gi.display()))?;
    }
    Ok(())
}

/// `profile.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileJson {
    pub version: u32,
    #[serde(default)]
    pub machines: Vec<String>,
}

pub fn load_profile_json(paths: &RematchPaths) -> Result<ProfileJson> {
    let p = dir(paths).join(PROFILE_JSON);
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProfileJson { version: PROFILE_VERSION, machines: Vec::new() }),
        Err(e) => Err(e).with_context(|| p.display().to_string()),
    }
}

/// Add `machine` to `profile.json` (sorted); `true` when the file changed.
pub fn note_machine(paths: &RematchPaths, machine: &str) -> Result<bool> {
    let p = dir(paths).join(PROFILE_JSON);
    let mut pj = load_profile_json(paths)?;
    let before = std::fs::read_to_string(&p).ok();
    pj.version = pj.version.max(PROFILE_VERSION);
    if !pj.machines.iter().any(|m| m == machine) {
        pj.machines.push(machine.to_string());
    }
    pj.machines.sort();
    pj.machines.dedup();
    let text = format!("{}\n", serde_json::to_string_pretty(&pj)?);
    if before.as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    std::fs::write(&p, text).with_context(|| format!("write {}", p.display()))?;
    Ok(true)
}

/// The selector `roles.yml` gives `role` on `machine`: a key equal to the role, or to the role
/// without its `rematch-` prefix (`editor` = `rematch-editor`); its value is a selector for every
/// machine, or a map `{default: <sel>, <hostname>: <sel>}` (the machine's key wins). `None` when
/// there is no profile, no file or no key.
pub fn role_selector(paths: &RematchPaths, role: &str, machine: &str) -> Result<Option<String>> {
    let f = roles_file(paths);
    let text = match std::fs::read_to_string(&f) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| f.display().to_string()),
    };
    let doc: BTreeMap<String, serde_yaml::Value> =
        serde_yaml::from_str::<Option<_>>(&text).map_err(|e| anyhow!("parse {}: {e}", f.display()))?.unwrap_or_default();
    let short = role.strip_prefix("rematch-");
    let Some(v) = doc.get(role).or_else(|| short.and_then(|s| doc.get(s))) else { return Ok(None) };
    let pick = match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Mapping(m) => m
            .get(machine)
            .or_else(|| m.get("default"))
            .and_then(|s| s.as_str())
            .map(str::to_string),
        _ => None,
    };
    Ok(pick.filter(|s| !s.trim().is_empty()))
}

/// Kept skill sources the shared dir and the agent dir link: `(name, dir)` of every
/// `profile/skills/<name>/` holding a `SKILL.md` (hidden names skipped), sorted.
pub fn kept_sources(paths: &RematchPaths) -> Vec<(String, PathBuf)> {
    if !present(paths) {
        return Vec::new();
    }
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(skills_dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let p = e.path();
            (!name.starts_with('.') && p.join(crate::library::SKILL_MD).is_file()).then_some((name, p))
        })
        .collect();
    v.sort();
    v
}

/// `profile/.index.json`: per `skills/<name>`, the machine and time of the last commit that
/// touched it (`git log -1 --format=%s -- skills/<name>` is `"<machine> <ts>"`), written by
/// every `profile sync` so `library ls`, `why` and `report` name the machine without git.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIndex {
    #[serde(default)]
    pub skills: BTreeMap<String, SkillOrigin>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOrigin {
    pub machine: String,
    /// The commit's `<ts>` (`%Y-%m-%dT%H:%M:%SZ`).
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub commit: String,
}

impl SkillOrigin {
    /// From a commit subject `"<machine> <ts>"` (a subject without a space is all machine).
    pub fn from_subject(subject: &str, commit: &str) -> SkillOrigin {
        let s = subject.trim();
        let (machine, at) = s.rsplit_once(' ').unwrap_or((s, ""));
        SkillOrigin { machine: machine.to_string(), at: at.to_string(), commit: commit.to_string() }
    }
}

/// The last sync's index; empty when there is none or it does not parse.
pub fn load_index(paths: &RematchPaths) -> ProfileIndex {
    std::fs::read_to_string(dir(paths).join(INDEX_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn write_index(paths: &RematchPaths, index: &ProfileIndex) -> Result<()> {
    let p = dir(paths).join(INDEX_FILE);
    let tmp = dir(paths).join(format!(".{INDEX_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(index)?)).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename into {}", p.display()))
}

/// Read the conflicts the last sync left (empty when none).
pub fn read_conflicts(paths: &RematchPaths) -> Vec<String> {
    std::fs::read_to_string(dir(paths).join(CONFLICTS_FILE))
        .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Record (or clear, with none) the conflicting paths of a sync.
pub fn write_conflicts(paths: &RematchPaths, conflicts: &[String]) -> Result<()> {
    let p = dir(paths).join(CONFLICTS_FILE);
    if conflicts.is_empty() {
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        return Ok(());
    }
    std::fs::write(&p, conflicts.iter().map(|c| format!("{c}\n")).collect::<String>())?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Links

/// What a sync did to the views (labels like `skills/<name>`, `agents/<file>`, `hooks/<file>`,
/// `policy.json`, `domains/<branch>.yml`, `improver/<file>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Views {
    /// Links created or re-pointed.
    pub linked: Vec<String>,
    /// Links removed: their profile target is gone.
    pub pruned: Vec<String>,
    /// A real file or a foreign link holds the name: left alone, the profile item is not linked there.
    pub untouched: Vec<String>,
}

/// What [`collect`] moved into the profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collected {
    /// Local files moved into the profile (a link left in their place).
    pub moved: Vec<String>,
    /// Local files kept because the profile already holds one of that name.
    pub kept_local: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    Same,
    Untouched,
}

/// `link` → `target`: created when absent, re-pointed when it is a link into the profile or a
/// dangling link; a real file or directory, or a link elsewhere, is the user's and stays.
pub fn ensure_link(paths: &RematchPaths, target: &Path, link: &Path) -> Result<LinkOutcome> {
    match std::fs::symlink_metadata(link) {
        Err(_) => {}
        Ok(md) if md.file_type().is_symlink() => {
            let cur = std::fs::read_link(link)?;
            if cur == target {
                return Ok(LinkOutcome::Same);
            }
            if !cur.starts_with(dir(paths)) && link.exists() {
                return Ok(LinkOutcome::Untouched);
            }
        }
        Ok(_) => return Ok(LinkOutcome::Untouched),
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    crate::library::relink(target, link)?;
    Ok(LinkOutcome::Linked)
}

/// Remove the links in `dir` that point under `under` at something gone.
fn prune_dangling(dir: &Path, under: &Path, label: &str, out: &mut Views) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    let mut names: Vec<(String, PathBuf)> =
        rd.flatten().filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path()))).collect();
    names.sort();
    for (name, p) in names {
        let Ok(t) = std::fs::read_link(&p) else { continue };
        if t.starts_with(under) && !p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
            out.pruned.push(format!("{label}/{name}"));
        }
    }
    Ok(())
}

fn children(dir: &Path, keep: impl Fn(&str, &Path) -> bool) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            let p = e.path();
            (!n.starts_with('.') && keep(&n, &p)).then_some((n, p))
        })
        .collect();
    v.sort();
    v
}

fn note(out: &mut Views, o: LinkOutcome, label: String) {
    match o {
        LinkOutcome::Linked => out.linked.push(label),
        LinkOutcome::Untouched => out.untouched.push(label),
        LinkOutcome::Same => {}
    }
}

/// sha256 of a file's bytes recorded at install time: `improver/.shipped/<name>`.
fn shipped_hash(paths: &RematchPaths, name: &str) -> Option<String> {
    std::fs::read_to_string(paths.improver.join(".shipped").join(name)).ok().map(|s| s.trim().to_string())
}

fn file_hash(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| crate::ledger::sha256_hex(&b))
}

/// A regular improver file whose content is not what this binary installed (its `.shipped`
/// record exists and differs): the user's edit.
fn improver_edited(paths: &RematchPaths, p: &Path, name: &str) -> bool {
    let regular = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file());
    regular && shipped_hash(paths, name).is_some_and(|h| file_hash(p).is_some_and(|c| c != h))
}

/// Link every profile item into the place its reader looks (no-op without a profile): kept
/// skills into `<agent_dir>/skills/<name>`, agents into `<agent_dir>/agents/<file>`, hooks
/// into `rematch-hooks/<id>.ts`, `config/policy.json` to `REMATCH_ROOT/policy.json`,
/// `config/domains/<b>.yml` to `branches/<b>/domain.yml`, `improver/<file>` over an untouched
/// shipped copy; links whose profile target is gone are pruned. The shared skills dir is the
/// library's (`library::sync_shared_skills` reads [`kept_sources`]).
pub fn link_views(paths: &RematchPaths) -> Result<Views> {
    let mut out = Views::default();
    if !present(paths) {
        return Ok(out);
    }
    for (name, src) in kept_sources(paths) {
        let o = ensure_link(paths, &src, &paths.skills_dir().join(&name))?;
        note(&mut out, o, format!("skills/{name}"));
    }
    prune_dangling(&paths.skills_dir(), &skills_dir(paths), "skills", &mut out)?;
    for (name, src) in children(&agents_dir(paths), |n, p| n.ends_with(".md") && p.is_file()) {
        let o = ensure_link(paths, &src, &paths.agents_dir().join(&name))?;
        note(&mut out, o, format!("agents/{name}"));
    }
    prune_dangling(&paths.agents_dir(), &agents_dir(paths), "agents", &mut out)?;
    for (name, src) in children(&hooks_dir(paths), |n, p| n.ends_with(".ts") && p.is_file()) {
        let o = ensure_link(paths, &src, &paths.hooks_dir().join(&name))?;
        note(&mut out, o, format!("hooks/{name}"));
    }
    prune_dangling(&paths.hooks_dir(), &hooks_dir(paths), "hooks", &mut out)?;
    let policy = policy_file(paths);
    let root_policy = paths.root.join(crate::domain::POLICY_FILE);
    if policy.is_file() {
        let o = ensure_link(paths, &policy, &root_policy)?;
        note(&mut out, o, crate::domain::POLICY_FILE.to_string());
    } else if std::fs::read_link(&root_policy).is_ok_and(|t| t.starts_with(dir(paths))) && !root_policy.exists() {
        std::fs::remove_file(&root_policy)?;
        out.pruned.push(crate::domain::POLICY_FILE.to_string());
    }
    for (name, src) in children(&domains_dir(paths), |n, p| n.ends_with(".yml") && p.is_file()) {
        let branch = name.trim_end_matches(".yml");
        if crate::library::validate_name(branch).is_err() {
            continue;
        }
        let o = ensure_link(paths, &src, &paths.branches.join(branch).join("domain.yml"))?;
        note(&mut out, o, format!("domains/{name}"));
    }
    for (b, d) in children(&paths.branches, |_, p| p.is_dir()) {
        let mut v = Views::default();
        prune_dangling(&d, &domains_dir(paths), &format!("domains/{b}"), &mut v)?;
        out.pruned.extend(v.pruned);
    }
    for (name, src) in children(&improver_dir(paths), |_, p| p.is_file()) {
        let link = paths.improver.join(&name);
        // An untouched shipped copy gives way to the profile's edit; the user's own edit stays.
        let pristine = std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_file()) && !improver_edited(paths, &link, &name);
        if pristine {
            std::fs::remove_file(&link)?;
        }
        let o = ensure_link(paths, &src, &link)?;
        note(&mut out, o, format!("improver/{name}"));
    }
    prune_dangling(&paths.improver, &improver_dir(paths), "improver", &mut out)?;
    Ok(out)
}

/// Move `src` (file or directory) to `dst` and leave a link `src → dst`.
pub fn move_and_link(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dst).is_err() {
        if src.is_dir() {
            copy_tree(src, dst)?;
            std::fs::remove_dir_all(src)?;
        } else {
            std::fs::copy(src, dst).with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
            std::fs::remove_file(src)?;
        }
    }
    crate::library::relink(dst, src)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let (s, d) = (ent.path(), dst.join(ent.file_name()));
        if ent.file_type()?.is_dir() {
            copy_tree(&s, &d)?;
        } else {
            std::fs::copy(&s, &d).with_context(|| format!("copy {}", s.display()))?;
        }
    }
    Ok(())
}

fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

fn collect_one(src: &Path, dst: &Path, label: String, out: &mut Collected) -> Result<()> {
    if !is_regular_file(src) {
        return Ok(());
    }
    if dst.exists() {
        out.kept_local.push(label);
        return Ok(());
    }
    move_and_link(src, dst)?;
    out.moved.push(label);
    Ok(())
}

/// Move this machine's local configuration into the profile (no-op without one), each leaving
/// a link: a regular `policy.json`, `branches/<b>/domain.yml` files, improver files the user
/// edited, and compiled hooks approved before the profile existed. A name the profile already
/// holds stays local (`kept_local`): the profile's copy is not overwritten.
pub fn collect(paths: &RematchPaths) -> Result<Collected> {
    let mut out = Collected::default();
    if !present(paths) {
        return Ok(out);
    }
    collect_one(&paths.root.join(crate::domain::POLICY_FILE), &policy_file(paths), crate::domain::POLICY_FILE.into(), &mut out)?;
    for (b, d) in children(&paths.branches, |_, p| p.is_dir()) {
        collect_one(&d.join("domain.yml"), &domains_dir(paths).join(format!("{b}.yml")), format!("domains/{b}.yml"), &mut out)?;
    }
    for (name, p) in children(&paths.improver, |_, p| p.is_file()) {
        if improver_edited(paths, &p, &name) {
            collect_one(&p, &improver_dir(paths).join(&name), format!("improver/{name}"), &mut out)?;
        }
    }
    for (name, p) in children(&paths.hooks_dir(), |n, _| n.ends_with(".ts")) {
        collect_one(&p, &hooks_dir(paths).join(&name), format!("hooks/{name}"), &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> (tempfile::TempDir, RematchPaths) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let paths = RematchPaths::with_roots(root.join("rematch"), root.join("agent"));
        paths.ensure_dirs().unwrap();
        std::fs::create_dir_all(dir(&paths).join(".git")).unwrap();
        ensure_layout(&paths).unwrap();
        (t, paths)
    }

    #[test]
    fn roles_per_machine_default_and_plain() {
        let (_t, paths) = env();
        assert_eq!(role_selector(&paths, "rematch-editor", "a").unwrap(), None);
        std::fs::write(
            roles_file(&paths),
            "editor: {default: anthropic/claude-opus-5-5, Vincent-M5M.local: openai/gpt-5}\nrematch-classifier: fake/small\n",
        )
        .unwrap();
        assert_eq!(role_selector(&paths, "rematch-editor", "Vincent-M5M.local").unwrap().as_deref(), Some("openai/gpt-5"));
        assert_eq!(role_selector(&paths, "rematch-editor", "other").unwrap().as_deref(), Some("anthropic/claude-opus-5-5"));
        assert_eq!(role_selector(&paths, "rematch-classifier", "other").unwrap().as_deref(), Some("fake/small"));
        assert_eq!(role_selector(&paths, "rematch-solver", "other").unwrap(), None);
    }

    #[test]
    fn gitignore_keeps_user_lines_and_machines_are_sorted() {
        let (_t, paths) = env();
        let gi = dir(&paths).join(".gitignore");
        std::fs::write(&gi, "mine\n").unwrap();
        ensure_layout(&paths).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(text.starts_with("mine\n") && text.contains("skills/*/rematch.json\n"), "{text}");
        assert!(note_machine(&paths, "b").unwrap());
        assert!(note_machine(&paths, "a").unwrap());
        assert!(!note_machine(&paths, "b").unwrap());
        assert_eq!(load_profile_json(&paths).unwrap(), ProfileJson { version: 1, machines: vec!["a".into(), "b".into()] });
    }

    #[test]
    fn collect_then_link_views_and_prune() {
        let (_t, paths) = env();
        // Local config moves into the profile, a link left behind.
        std::fs::write(paths.root.join("policy.json"), "{}\n").unwrap();
        std::fs::create_dir_all(paths.branches.join("payments")).unwrap();
        std::fs::write(paths.branches.join("payments/domain.yml"), "domain: payments\n").unwrap();
        std::fs::create_dir_all(paths.improver.join(".shipped")).unwrap();
        std::fs::write(paths.improver.join("editor.md"), "edited\n").unwrap();
        std::fs::write(paths.improver.join(".shipped/editor.md"), crate::ledger::sha256_hex(b"shipped\n")).unwrap();
        std::fs::write(paths.improver.join("lint.toml"), "same\n").unwrap();
        std::fs::write(paths.improver.join(".shipped/lint.toml"), crate::ledger::sha256_hex(b"same\n")).unwrap();
        let c = collect(&paths).unwrap();
        assert_eq!(c.moved, vec!["policy.json", "domains/payments.yml", "improver/editor.md"]);
        assert_eq!(std::fs::read_link(paths.root.join("policy.json")).unwrap(), policy_file(&paths));
        assert_eq!(std::fs::read_link(paths.branches.join("payments/domain.yml")).unwrap(), domains_dir(&paths).join("payments.yml"));
        assert!(is_regular_file(&paths.improver.join("lint.toml")), "untouched shipped file stays");

        // Profile items from another machine are linked; a real dir of the same name stays.
        std::fs::create_dir_all(skills_dir(&paths).join("a")).unwrap();
        std::fs::write(skills_dir(&paths).join("a/SKILL.md"), "---\nname: a\n---\n").unwrap();
        std::fs::create_dir_all(skills_dir(&paths).join("b")).unwrap();
        std::fs::write(skills_dir(&paths).join("b/SKILL.md"), "---\nname: b\n---\n").unwrap();
        std::fs::create_dir_all(paths.skills_dir().join("b")).unwrap();
        std::fs::write(agents_dir(&paths).join("r.md"), "---\nname: r\n---\n").unwrap();
        std::fs::write(hooks_dir(&paths).join("h1.ts"), "export default {};\n").unwrap();
        std::fs::write(domains_dir(&paths).join("opts.yml"), "domain: quant\n").unwrap();
        std::fs::write(improver_dir(&paths).join("lint.toml"), "theirs\n").unwrap();
        let v = link_views(&paths).unwrap();
        assert_eq!(v.linked, vec!["skills/a", "agents/r.md", "hooks/h1.ts", "domains/opts.yml", "improver/lint.toml"], "{v:?}");
        assert_eq!(v.untouched, vec!["skills/b"]);
        assert_eq!(std::fs::read_to_string(paths.improver.join("lint.toml")).unwrap(), "theirs\n");
        assert_eq!(kept_sources(&paths).len(), 2);
        assert_eq!(link_views(&paths).unwrap(), Views { untouched: vec!["skills/b".into()], ..Default::default() }, "idempotent");

        // Deleted upstream → pruned.
        std::fs::remove_dir_all(skills_dir(&paths).join("a")).unwrap();
        std::fs::remove_file(hooks_dir(&paths).join("h1.ts")).unwrap();
        let v = link_views(&paths).unwrap();
        assert_eq!(v.pruned, vec!["skills/a", "hooks/h1.ts"]);
        assert!(std::fs::symlink_metadata(paths.skills_dir().join("a")).is_err());
    }

    #[test]
    fn conflicts_round_trip() {
        let (_t, paths) = env();
        assert!(read_conflicts(&paths).is_empty());
        write_conflicts(&paths, &["skills/a/SKILL.md".into()]).unwrap();
        assert_eq!(read_conflicts(&paths), vec!["skills/a/SKILL.md"]);
        write_conflicts(&paths, &[]).unwrap();
        assert!(!dir(&paths).join(CONFLICTS_FILE).exists());
    }
}
