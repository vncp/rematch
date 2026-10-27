//! `rematch policy show | domain set|unset | caps set | scope set|unset` (DESIGN §6 "Domain",
//! §8, §16 "Scoped exposure"): the one writer of `REMATCH_ROOT/policy.json`. Edits keep every
//! other key as found and replace the file atomically (temp file + rename). Nothing else in the
//! binary writes it.
//!
//! Also the digest's domain proposal ([`domain_suggestions`]): an undeclared remote whose
//! sessions keep reading skills of one domain gets one informational line naming the
//! `policy domain set` that would declare it; it is never applied automatically.
//!
//! `scope set <name> <target>` writes `scopes.<name>` (`domain::PlaceScope`): the skill of that
//! name leaves every location all sessions load and reaches only the matching place's view and
//! repo trials (`library::sync_view`, `library::sync_trials`). User-only: the model-facing
//! guards (`extension/index.ts` `userOnlyReason`) refuse it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use rematch_core::domain::{
    common_domain, normalize_domain, session_domain, top_domain, DomainDecl, DomainMatch, DomainPolicy, PlaceScope, POLICY_FILE,
};
use rematch_core::identity;
use rematch_core::import::repo_of;
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::predicate::{escape_glob, ANY_REPO};
use rematch_core::{library, profile, Kind, Status};
use serde_json::{json, Map, Value};

use crate::app::AppCtx;

/// Sessions in one undeclared remote, reading skills of one domain, before the digest
/// proposes declaring it.
pub const MIN_DOMAIN_SUGGEST_SESSIONS: usize = 5;

#[derive(Subcommand, Debug, Clone)]
pub enum PolicyCmd {
    /// The policy file, the declared domains, caps and skill scopes in effect, and the branch domain.yml domains.
    Show,
    /// Declare (set) or remove (unset) the domain of a repo remote (or remote glob, or a repo path).
    Domain {
        #[command(subcommand)]
        cmd: DomainCmd,
    },
    /// Caps (`kept_per_domain`: kept entries per top-level domain across branches, default 8).
    Caps {
        #[command(subcommand)]
        cmd: CapsCmd,
    },
    /// Place scope of a skill name: exposed only in one repo (by remote, any clone or worktree) or
    /// directory, never globally (set|unset; reserved to the user).
    Scope {
        #[command(subcommand)]
        cmd: ScopeCmd,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ScopeCmd {
    /// Expose skill `<name>` only at `<target>`: a repo path on this machine (stored as its normalized remote, so every clone and worktree matches), a normalized remote (`github.com/org/repo`), or a directory outside git (stored as its path). Withdraws the name from REMATCH_ROOT/skills and <agent_dir>/skills, rewrites the place of its live entries and resyncs.
    Set {
        name: String,
        target: String,
        /// Only below this toplevel-relative path glob (default `**`, the whole repo).
        #[arg(long)]
        subpath: Option<String>,
    },
    /// Remove the scope of `<name>`: its kept and profile copies are global again (entry predicates keep the scoped place).
    Unset { name: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DomainCmd {
    /// Declare `<domain>` for sessions in `<target>` (a normalized remote, a glob over remotes, or a repo path on this machine).
    Set {
        target: String,
        domain: String,
        /// Only below this toplevel-relative path glob.
        #[arg(long)]
        subpath: Option<String>,
    },
    /// Remove the declaration for `<target>` (and `--subpath`).
    Unset {
        target: String,
        #[arg(long)]
        subpath: Option<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum CapsCmd {
    /// Set a cap.
    Set { cap: Cap, value: usize },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum Cap {
    #[value(name = "kept_per_domain")]
    KeptPerDomain,
}

/// `REMATCH_ROOT/policy.json`, or with a profile the file its link points to
/// (`profile/config/policy.json`, DESIGN §20), so the atomic rename replaces the file, not the link.
fn policy_path(ctx: &AppCtx) -> PathBuf {
    let p = ctx.paths.root.join(POLICY_FILE);
    match std::fs::read_link(&p) {
        Ok(t) if t.is_absolute() => t,
        Ok(t) => ctx.paths.root.join(t),
        Err(_) => p,
    }
}

/// The policy file as a JSON object (`{}` when absent).
fn read_policy(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) => match serde_json::from_str(&t).with_context(|| format!("parse {}", path.display()))? {
            Value::Object(m) => Ok(m),
            _ => bail!("{} is not a JSON object", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Atomic replace: a sibling temp file, fsync, rename.
fn write_policy(path: &Path, m: &Map<String, Value>) -> Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{POLICY_FILE}.tmp-{}", std::process::id()));
    let mut text = serde_json::to_string_pretty(m)?;
    text.push('\n');
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// A `domain set|unset` target as stored: a repo path on this machine → its remote; a glob
/// stays as written; anything else is normalized like a recorded remote.
fn resolve_target(ctx: &AppCtx, target: &str) -> Result<String> {
    let t = target.trim();
    if t.is_empty() {
        bail!("empty target");
    }
    let path = crate::app::absolute(&ctx.cwd.join(t))?;
    if path.is_dir() {
        let top = rematch_core::import::repo_of(&path).ok_or_else(|| anyhow!("{} is not inside a git worktree", path.display()))?;
        return identity::repo_remote_of(&top).ok_or_else(|| anyhow!("{} has no readable git config", top.display()));
    }
    if t.contains(['*', '?', '[']) {
        return Ok(t.to_string());
    }
    Ok(identity::normalize_remote(t).unwrap_or_else(|| t.to_string()))
}

/// More specific declarations first (a subpath, then an exact remote), else file order:
/// `domains` is first-match-wins.
fn specificity(d: &DomainDecl) -> u8 {
    let exact = d.when.repo_remote.as_deref().is_some_and(|r| !r.contains(['*', '?', '[']));
    (d.when.subpath.is_some() as u8) * 2 + exact as u8
}

fn decls(m: &Map<String, Value>) -> Result<Vec<DomainDecl>> {
    match m.get("domains") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).context("policy.json domains"),
    }
}

/// Recorded sessions whose domain the policy at `ctx` now decides differently (what the next
/// `lake migrate` rewrites).
fn sessions_to_reapply(ctx: &AppCtx) -> Result<usize> {
    let policy = ctx.domain_policy()?;
    Ok(ctx.ledger.list_sessions(&SessionFilter::default())?.iter().filter(|s| session_domain(s, &policy) != s.domain).count())
}

pub fn cmd_policy(ctx: &AppCtx, cmd: PolicyCmd) -> Result<Value> {
    let path = policy_path(ctx);
    match cmd {
        PolicyCmd::Show => show(ctx, &path),
        PolicyCmd::Domain { cmd } => {
            let mut m = read_policy(&path)?;
            let mut list = decls(&m)?;
            let (target, subpath, line) = match cmd {
                DomainCmd::Set { target, domain, subpath } => {
                    let d = normalize_domain(&domain)
                        .ok_or_else(|| anyhow!("bad domain {domain:?}: dotted lowercase segments of [a-z0-9_-], e.g. web.frontend"))?;
                    let remote = resolve_target(ctx, &target)?;
                    let when = DomainMatch { repo_remote: Some(remote.clone()), subpath: subpath.clone() };
                    match list.iter_mut().find(|x| x.when == when) {
                        Some(x) => x.domain = d.clone(),
                        None => list.push(DomainDecl { when, domain: d.clone() }),
                    }
                    list.sort_by_key(|x| std::cmp::Reverse(specificity(x)));
                    let at = subpath.as_deref().map(|s| format!(" (subpath {s})")).unwrap_or_default();
                    (remote.clone(), subpath, format!("declared domain {d} for {remote}{at}"))
                }
                DomainCmd::Unset { target, subpath } => {
                    let remote = resolve_target(ctx, &target)?;
                    let when = DomainMatch { repo_remote: Some(remote.clone()), subpath: subpath.clone() };
                    let before = list.len();
                    list.retain(|x| x.when != when);
                    if list.len() == before {
                        bail!("no domain declared for {remote}{}", subpath.as_deref().map(|s| format!(" (subpath {s})")).unwrap_or_default());
                    }
                    (remote.clone(), subpath, format!("removed the domain declaration for {remote}"))
                }
            };
            m.insert("domains".into(), serde_json::to_value(&list)?);
            write_policy(&path, &m)?;
            let pending = sessions_to_reapply(ctx)?;
            Ok(json!({
                "path": path, "target": target, "subpath": subpath, "domains": list, "sessions_to_reapply": pending,
                "lines": [line, format!("{pending} recorded session(s) change domain: `rematch lake migrate` reapplies the policy to sessions and entry predicates")],
            }))
        }
        PolicyCmd::Caps { cmd: CapsCmd::Set { cap: Cap::KeptPerDomain, value } } => {
            if value == 0 {
                bail!("kept_per_domain must be ≥ 1");
            }
            let mut m = read_policy(&path)?;
            let caps = m.entry("caps").or_insert_with(|| json!({}));
            let Value::Object(caps) = caps else { bail!("policy.json caps is not an object") };
            caps.insert("kept_per_domain".into(), json!(value));
            write_policy(&path, &m)?;
            Ok(json!({"path": path, "caps": m["caps"], "lines": [format!("kept_per_domain = {value}")]}))
        }
        PolicyCmd::Scope { cmd: ScopeCmd::Set { name, target, subpath } } => scope_set(ctx, &path, &name, &target, subpath.as_deref()),
        PolicyCmd::Scope { cmd: ScopeCmd::Unset { name } } => scope_unset(ctx, &path, &name),
    }
}

/// A `scope set` target as stored, plus the `repo_glob` the scoped entries' predicates take
/// (their place test for a session without a remote): a repo path → its remote (subpath `**`
/// unless given) and its escaped toplevel; a directory outside git → its escaped path (both); a
/// remote → itself and the toplevel of the newest recorded session of that remote still on
/// disk, else the escaped remote (a glob no absolute path matches: only the remote decides).
fn resolve_scope(ctx: &AppCtx, target: &str, subpath: Option<&str>) -> Result<(PlaceScope, String)> {
    let t = target.trim();
    if t.is_empty() {
        bail!("empty target");
    }
    let subpath = subpath.map(|s| s.trim().trim_matches('/')).filter(|s| !s.is_empty());
    let path = crate::app::absolute(&ctx.cwd.join(t))?;
    let (scope, glob) = if path.is_dir() {
        match repo_of(&path) {
            Some(top) => {
                let remote = identity::repo_remote_of(&top).ok_or_else(|| anyhow!("{} has no readable git config", top.display()))?;
                let scope = PlaceScope { remote: Some(remote), subpath: Some(subpath.unwrap_or(ANY_REPO).to_string()), repo_glob: None };
                (scope, escape_glob(&top.to_string_lossy()))
            }
            None => {
                let glob = escape_glob(&path.to_string_lossy());
                (PlaceScope { remote: None, subpath: subpath.map(str::to_string), repo_glob: Some(glob.clone()) }, glob)
            }
        }
    } else {
        let pathlike = t.starts_with(['/', '.', '~', '\\']) || (t.len() >= 2 && t.as_bytes()[1] == b':' && t.as_bytes()[0].is_ascii_alphabetic());
        if pathlike {
            bail!("{} is not a directory on this machine", path.display());
        }
        let remote = identity::normalize_remote(t)
            .filter(|r| r.contains('/') || r.starts_with("local:"))
            .ok_or_else(|| anyhow!("{t:?} is neither a directory on this machine nor a git remote (host/org/repo)"))?;
        let local = ctx
            .ledger
            .list_sessions(&SessionFilter::default())?
            .into_iter()
            .filter(|s| s.repo_remote.as_deref() == Some(remote.as_str()))
            .filter_map(|s| s.repo.filter(|r| Path::new(r).is_dir()).map(|r| (s.started_at, r)))
            .max();
        let glob = local.map(|(_, r)| escape_glob(&r)).unwrap_or_else(|| escape_glob(&remote));
        (PlaceScope { remote: Some(remote), subpath: Some(subpath.unwrap_or(ANY_REPO).to_string()), repo_glob: None }, glob)
    };
    scope.check()?;
    Ok((scope, glob))
}

fn scopes_of(m: &Map<String, Value>) -> Result<BTreeMap<String, PlaceScope>> {
    match m.get("scopes") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(v) => serde_json::from_value(v.clone()).context("policy.json scopes"),
    }
}

/// Where skill `name` is exposed to every session right now: the shared dir link, and
/// `<agent_dir>/skills/<name>` (a link rematch made into the profile, or something else there).
fn global_exposure(ctx: &AppCtx, name: &str) -> (Option<PathBuf>, Option<PathBuf>, bool) {
    let shared = ctx.paths.skills_shared.join(name);
    let native = ctx.paths.skills_dir().join(name);
    let shared = std::fs::symlink_metadata(&shared).is_ok().then_some(shared);
    let ours = std::fs::read_link(&native).is_ok_and(|t| t.starts_with(profile::dir(&ctx.paths)));
    let present = std::fs::symlink_metadata(&native).is_ok();
    (shared, present.then_some(native), ours)
}

const RELOAD_LINE: &str =
    "sessions already running keep the skill text they loaded; new sessions (and omp after /reload) see the change";

/// `policy scope set`: write the scope, rewrite the place fields of every live entry of that
/// name to it (so counting and the predicate machinery agree), withdraw every view's link of the
/// name (each place relinks it at its next sync if its scope matches), then resync the profile
/// links and the kept library, which leave the name out. Reports what was withdrawn and what
/// stays global because rematch did not make it.
fn scope_set(ctx: &AppCtx, path: &Path, name: &str, target: &str, subpath: Option<&str>) -> Result<Value> {
    library::validate_name(name)?;
    let (scope, glob) = resolve_scope(ctx, target, subpath)?;
    let (shared_before, native_before, _) = global_exposure(ctx, name);
    let mut m = read_policy(path)?;
    let mut scopes = scopes_of(&m)?;
    scopes.insert(name.to_string(), scope.clone());
    m.insert("scopes".into(), serde_json::to_value(&scopes)?);
    write_policy(path, &m)?;

    let mut rewritten = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter { name: Some(name.to_string()), ..Default::default() })? {
        if !matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned) {
            continue;
        }
        let mut next = e.clone();
        next.predicate.repo_glob = glob.clone();
        next.predicate.repo_remote = scope.remote.clone();
        next.predicate.subpath = scope.remote.as_ref().and(scope.subpath.clone());
        if next != e {
            crate::scope::save_entry(ctx, &next)?;
            rewritten.push(json!({"id": next.id, "status": next.status, "path": next.path}));
        }
    }
    let views = library::unlink_from_views(&ctx.paths, name)?;
    let profile_views = profile::link_views(&ctx.paths)?;
    library::sync_shared_skills(&ctx.ledger, &ctx.paths)?;
    let (shared_after, native_after, native_ours) = global_exposure(ctx, name);

    let mut withdrawn: Vec<String> = Vec::new();
    if let (Some(p), None) = (&shared_before, &shared_after) {
        withdrawn.push(p.display().to_string());
    }
    if let (Some(p), None) = (&native_before, &native_after) {
        withdrawn.push(p.display().to_string());
    }
    if views > 0 {
        withdrawn.push(format!("{views} view link(s) under {}", library::views_dir(&ctx.paths).display()));
    }
    let mut still_global: Vec<String> = Vec::new();
    if let Some(p) = &shared_after {
        still_global.push(format!("{} is not a rematch link: left alone", p.display()));
    }
    if let Some(p) = native_after.as_ref().filter(|_| !native_ours) {
        still_global.push(format!(
            "{} is a real directory or a link rematch did not make: omp loads it in every session; move it into the profile (`rematch profile adopt {}`) to scope it",
            p.display(),
            p.display()
        ));
    }
    for e in ctx.ledger.list_entries(&EntryFilter { name: Some(name.to_string()), ..Default::default() })? {
        if e.kind == Kind::Agent && matches!(e.status, Status::Kept | Status::Pinned) {
            still_global.push(format!("kept agent {} ({}) stays in {}: agents have no per-place mount", e.name, e.id, ctx.paths.agents_dir().display()));
        }
    }
    let mut lines = vec![format!("scoped {name} to {} (policy.json scopes; travels with the profile)", scope.label())];
    lines.push(match rewritten.len() {
        0 => "no live entry of that name to re-place".to_string(),
        n => format!("re-placed {n} live entr{} to the scope (repo_glob {glob})", if n == 1 { "y" } else { "ies" }),
    });
    lines.push(if withdrawn.is_empty() { "withdrawn: nothing was exposed globally".to_string() } else { format!("withdrawn: {}", withdrawn.join("; ")) });
    lines.extend(still_global.iter().map(|s| format!("still global: {s}")));
    lines.push(format!("sessions in the scoped place get {name} through their view at their next start (or `rematch skills sync` there)"));
    lines.push(RELOAD_LINE.to_string());
    Ok(json!({
        "path": path, "name": name, "scope": scope, "rewritten": rewritten, "withdrawn": withdrawn,
        "still_global": still_global, "views": profile_views, "lines": lines,
    }))
}

/// `policy scope unset`: remove the scope; the resync links the name's kept and profile copies
/// into every global location again. Entry predicates keep the place `scope set` gave them.
fn scope_unset(ctx: &AppCtx, path: &Path, name: &str) -> Result<Value> {
    let mut m = read_policy(path)?;
    let mut scopes = scopes_of(&m)?;
    let Some(old) = scopes.remove(name) else { bail!("no scope for {name}") };
    m.insert("scopes".into(), serde_json::to_value(&scopes)?);
    write_policy(path, &m)?;
    let profile_views = profile::link_views(&ctx.paths)?;
    library::sync_shared_skills(&ctx.ledger, &ctx.paths)?;
    let (shared, native, _) = global_exposure(ctx, name);
    let restored: Vec<String> = [shared, native].into_iter().flatten().map(|p| p.display().to_string()).collect();
    let lines = vec![
        format!("removed the scope of {name} ({})", old.label()),
        if restored.is_empty() { format!("{name} has no kept or profile copy: nothing global to restore") } else { format!("global again: {}", restored.join("; ")) },
        "entry predicates keep the place `scope set` gave them".to_string(),
        RELOAD_LINE.to_string(),
    ];
    Ok(json!({"path": path, "name": name, "removed": old, "restored": restored, "views": profile_views, "lines": lines}))
}

fn show(ctx: &AppCtx, path: &Path) -> Result<Value> {
    let raw = read_policy(path)?;
    let p = ctx.domain_policy()?;
    let mut lines = vec![format!("policy {}{}", path.display(), if path.is_file() { "" } else { " (absent)" })];
    if p.declared.is_empty() {
        lines.push("domains: none declared".into());
    }
    for d in &p.declared {
        lines.push(format!(
            "domain {} ← remote {}{}",
            d.domain,
            d.when.repo_remote.as_deref().unwrap_or("*"),
            d.when.subpath.as_deref().map(|s| format!(" subpath {s}")).unwrap_or_default()
        ));
    }
    for (b, d) in &p.branch_domains {
        lines.push(format!("domain {d} ← branches/{b}/domain.yml"));
    }
    lines.push(format!("caps: kept_per_domain {}", p.kept_per_domain()));
    if p.scopes.is_empty() {
        lines.push("scopes: none (every kept skill is global)".into());
    }
    for (name, s) in &p.scopes {
        lines.push(format!("scope {name} → {} only", s.label()));
    }
    Ok(json!({
        "path": path, "policy": Value::Object(raw), "declared": p.declared, "branch_domains": p.branch_domains,
        "kept_per_domain": p.kept_per_domain(), "scopes": p.scopes, "lines": lines,
    }))
}

/// The digest's domain proposals: per remote with no declaration, each session whose read
/// skills map to entries of one domain (`common_domain` of their predicates) votes for that
/// domain's top level; a remote where one top level has ≥ [`MIN_DOMAIN_SUGGEST_SESSIONS`]
/// votes (and no other as many) gets `{remote, domain, sessions, command}`, `domain` being
/// the `common_domain` of its voters (`web.frontend` and `web.a11y` readers ⇒ `web`).
pub fn domain_suggestions(ctx: &AppCtx, policy: &DomainPolicy) -> Result<Vec<Value>> {
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        if let Some(d) = e.predicate.domain {
            by_name.entry(e.name).or_default().push(d);
        }
    }
    // remote → top level → the voters' domains
    let mut votes: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for s in ctx.ledger.list_sessions(&SessionFilter::default())? {
        let Some(remote) = s.repo_remote.as_deref() else { continue };
        if policy.declared_for(Some(remote), s.subpath.as_deref()).is_some() {
            continue;
        }
        let read: Vec<&str> =
            s.skill_reads.iter().filter_map(|n| by_name.get(n)).flat_map(|ds| ds.iter().map(String::as_str)).collect();
        if let Some(d) = common_domain(read.iter().map(|d| Some(*d))) {
            votes.entry(remote.to_string()).or_default().entry(top_domain(&d).to_string()).or_default().push(d);
        }
    }
    let mut out = Vec::new();
    for (remote, per) in votes {
        let mut ranked: Vec<Vec<String>> = per.into_values().collect();
        ranked.sort_by_key(|v| std::cmp::Reverse(v.len()));
        let n = ranked[0].len();
        let tied = ranked.get(1).is_some_and(|r| r.len() == n);
        if n < MIN_DOMAIN_SUGGEST_SESSIONS || tied {
            continue;
        }
        let Some(d) = common_domain(ranked[0].iter().map(|d| Some(d.as_str()))) else { continue };
        let command = format!("rematch policy domain set {remote} {d}");
        out.push(json!({"remote": remote, "domain": d, "sessions": n, "command": command}));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, session, stage};

    fn run(ctx: &AppCtx, cmd: PolicyCmd) -> Value {
        cmd_policy(ctx, cmd).unwrap()
    }

    fn set(target: &str, domain: &str, subpath: Option<&str>) -> PolicyCmd {
        PolicyCmd::Domain { cmd: DomainCmd::Set { target: target.into(), domain: domain.into(), subpath: subpath.map(str::to_string) } }
    }

    #[test]
    fn set_unset_and_caps_edit_the_file_keeping_other_keys() {
        let t = env();
        let ctx = &t.ctx;
        let path = ctx.paths.root.join(POLICY_FILE);
        std::fs::write(&path, r#"{"environment":"work","skip_profiles":["sandbox"]}"#).unwrap();
        run(ctx, set("https://github.com/Acme/Web.git", "Web", None));
        run(ctx, set("github.com/acme/*", "quant", None));
        run(ctx, set("github.com/acme/web", "web.frontend", Some("ui")));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((v["environment"].as_str(), &v["skip_profiles"]), (Some("work"), &json!(["sandbox"])));
        assert_eq!(
            v["domains"],
            json!([
                {"match": {"repo_remote": "github.com/acme/web", "subpath": "ui"}, "domain": "web.frontend"},
                {"match": {"repo_remote": "github.com/acme/web"}, "domain": "web"},
                {"match": {"repo_remote": "github.com/acme/*"}, "domain": "quant"},
            ]),
            "normalized, most specific first"
        );
        // set again replaces in place
        run(ctx, set("github.com/acme/web", "mobile", None));
        let p = ctx.domain_policy().unwrap();
        assert_eq!(p.declared_for(Some("github.com/acme/web"), Some("")).unwrap().domain, "mobile");
        assert_eq!(p.declared_for(Some("github.com/acme/web"), Some("ui/x")).unwrap().domain, "web.frontend");
        assert_eq!(p.declared_for(Some("github.com/acme/api"), None).unwrap().domain, "quant");
        run(ctx, PolicyCmd::Domain { cmd: DomainCmd::Unset { target: "github.com/acme/*".into(), subpath: None } });
        assert!(ctx.domain_policy().unwrap().declared_for(Some("github.com/acme/api"), None).is_none());
        assert!(cmd_policy(ctx, PolicyCmd::Domain { cmd: DomainCmd::Unset { target: "github.com/x/y".into(), subpath: None } }).is_err());
        assert!(cmd_policy(ctx, set("github.com/x/y", "a/b", None)).is_err(), "bad domain");
        run(ctx, PolicyCmd::Caps { cmd: CapsCmd::Set { cap: Cap::KeptPerDomain, value: 3 } });
        assert_eq!(ctx.domain_policy().unwrap().kept_per_domain(), 3);
        let shown = run(ctx, PolicyCmd::Show);
        assert_eq!(shown["kept_per_domain"], 3);
        assert_eq!(shown["policy"]["environment"], "work");
        assert!(!ctx.paths.root.read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".tmp-")), "no temp file left");
        // the export policy still parses the file this command wrote
        assert!(crate::scope::export::Policy::load(&ctx.paths.root).is_ok());
    }

    #[test]
    fn digest_proposes_a_domain_for_an_undeclared_remote_reading_one_domains_skills() {
        let t = env();
        let ctx = &t.ctx;
        let mut web = entry("e-web", "web-review", "app");
        web.predicate.domain = Some("web.frontend".into());
        stage(ctx, &web, &[]);
        let mut a11y = entry("e-a11y", "a11y-check", "app");
        a11y.predicate.domain = Some("web.a11y".into());
        stage(ctx, &a11y, &[]);
        for i in 0..5 {
            let mut s = session(&format!("s{i}"), "app", Some("/w/app"), "anthropic/claude-opus", 1_000 + i);
            s.repo_remote = Some("github.com/acme/app".into());
            s.skill_reads = if i % 2 == 0 { vec!["web-review".into()] } else { vec!["web-review".into(), "a11y-check".into()] };
            ctx.ledger.upsert_session(&s).unwrap();
        }
        let p = ctx.domain_policy().unwrap();
        let got = domain_suggestions(ctx, &p).unwrap();
        assert_eq!(got, vec![json!({"remote": "github.com/acme/app", "domain": "web", "sessions": 5, "command": "rematch policy domain set github.com/acme/app web"})]);
        let digest = crate::digest::cmd_digest(ctx).unwrap();
        assert!(
            digest["lines"].as_array().unwrap().iter().any(|l| l == "declare domain web for github.com/acme/app: rematch policy domain set github.com/acme/app web"),
            "{digest}"
        );
        // declared: no proposal; never written by the digest
        run(ctx, set("github.com/acme/app", "web", None));
        assert!(domain_suggestions(ctx, &ctx.domain_policy().unwrap()).unwrap().is_empty());
    }

    fn scope(name: &str, target: &str, subpath: Option<&str>) -> PolicyCmd {
        PolicyCmd::Scope { cmd: ScopeCmd::Set { name: name.into(), target: target.into(), subpath: subpath.map(str::to_string) } }
    }

    #[test]
    fn scope_set_writes_the_scope_replaces_live_entries_and_withdraws_global_links() {
        let t = env();
        let ctx = &t.ctx;
        let base = rematch_core::paths::canonicalize(&t.dir).unwrap();
        let web = base.join("web");
        std::fs::create_dir_all(web.join(".git")).unwrap();
        std::fs::write(web.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:Acme/Web.git\n").unwrap();
        std::fs::write(ctx.paths.root.join(POLICY_FILE), r#"{"environment":"work"}"#).unwrap();
        // A provisional entry everywhere, and a kept one of the same name: no profile, so its
        // dir is <agent_dir>/skills/<name> itself, which rematch cannot withdraw.
        let p = stage(ctx, &entry("e-p", "bellwether-dev", "shared"), &[]);
        let k = stage(ctx, &entry("e-k", "bellwether-dev", "web"), &[]);
        let k = crate::scope::testutil::keep(ctx, &k.id);
        let shared_link = ctx.paths.skills_shared.join("bellwether-dev");
        assert!(std::fs::read_link(&shared_link).is_ok());

        let out = run(ctx, scope("bellwether-dev", &web.to_string_lossy(), None));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(ctx.paths.root.join(POLICY_FILE)).unwrap()).unwrap();
        assert_eq!(v["environment"], "work", "other keys stay");
        assert_eq!(v["scopes"], json!({"bellwether-dev": {"remote": "github.com/acme/web", "subpath": "**"}}));
        let top = escape_glob(&web.to_string_lossy());
        for id in [&p.id, &k.id] {
            let e = ctx.ledger.get_entry(id).unwrap().unwrap();
            assert_eq!((e.predicate.repo_glob.as_str(), e.predicate.repo_remote.as_deref(), e.predicate.subpath.as_deref()), (top.as_str(), Some("github.com/acme/web"), Some("**")), "{id}");
            let mirror = library::load_entry(&e.path).unwrap();
            assert_eq!(mirror.predicate, e.predicate, "rematch.json follows");
        }
        assert_eq!(out["rewritten"].as_array().unwrap().len(), 2, "{out}");
        assert!(std::fs::symlink_metadata(&shared_link).is_err());
        assert_eq!(out["withdrawn"], json!([shared_link.display().to_string()]));
        assert!(out["still_global"][0].as_str().unwrap().contains("real directory"), "{out}");
        let lines = out["lines"].to_string();
        assert!(lines.contains("still global") && lines.contains("keep the skill text they loaded"), "{lines}");
        assert!(run(ctx, PolicyCmd::Show)["lines"].to_string().contains("scope bellwether-dev → remote github.com/acme/web subpath ** only"));

        // A remote target: the newest recorded checkout of it gives the entries' glob, else the remote itself.
        let mut s = session("s1", "web", Some(&web.to_string_lossy()), "anthropic/claude-opus", 1_000);
        s.repo_remote = Some("github.com/acme/web".into());
        ctx.ledger.upsert_session(&s).unwrap();
        stage(ctx, &entry("e-r", "remote-skill", "shared"), &[]);
        run(ctx, scope("remote-skill", "https://github.com/Acme/Web.git", Some("ui/")));
        let e = ctx.ledger.get_entry("e-r").unwrap().unwrap();
        assert_eq!((e.predicate.repo_glob.as_str(), e.predicate.subpath.as_deref()), (top.as_str(), Some("ui")));
        stage(ctx, &entry("e-u", "unseen", "shared"), &[]);
        run(ctx, scope("unseen", "gitlab.com/x/y", None));
        assert_eq!(ctx.ledger.get_entry("e-u").unwrap().unwrap().predicate.repo_glob, "gitlab.com/x/y");
        // A directory outside git is scoped by its path.
        let notes = base.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let out = run(ctx, scope("notes-skill", &notes.to_string_lossy(), None));
        assert_eq!(out["scope"], json!({"repo_glob": escape_glob(&notes.to_string_lossy())}));

        for (name, target, sub) in [
            ("x", "/no/such/dir", None),
            ("x", "not-a-remote", None),
            ("x", "github.com/acme/*", None),
            ("x", notes.to_str().unwrap(), Some("sub")),
            ("../x", web.to_str().unwrap(), None),
        ] {
            assert!(cmd_policy(ctx, scope(name, target, sub)).is_err(), "{name} {target} {sub:?}");
        }

        let out = run(ctx, PolicyCmd::Scope { cmd: ScopeCmd::Unset { name: "bellwether-dev".into() } });
        assert_eq!(out["restored"][0], json!(shared_link.display().to_string()), "{out}");
        assert!(std::fs::read_link(&shared_link).is_ok(), "global again");
        assert!(cmd_policy(ctx, PolicyCmd::Scope { cmd: ScopeCmd::Unset { name: "bellwether-dev".into() } }).is_err());
    }
}
