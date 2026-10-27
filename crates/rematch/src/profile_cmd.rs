//! `profile init [--remote <url>] [--adopt] | adopt <path> | sync [--no-push] | status |
//! install-launchd [--interval 300]` (DESIGN §20): `REMATCH_ROOT/profile/` is a git working tree
//! holding one person's omp setup (kept skills and agents, adopted hand-written ones, approved
//! hooks, approved personal instructions, policy, roles, domain files, edited improver prompts),
//! synced across machines through
//! a git remote. The layout, the views linked into omp/rematch and `roles.yml` are
//! `rematch_core::profile`; this module runs git: local commands unbounded, network commands
//! (`clone`, `fetch`, `push`) inside one `--timeout` budget that fails open (`offline: true`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use rematch_core::ledger::EntryFilter;
use rematch_core::library::{self, AGENTS_FRAGMENT, SHARED_BRANCH, SKILL_MD};
use rematch_core::outcome::machine_name;
use rematch_core::package::SkillDoc;
use rematch_core::profile;
use rematch_core::types::{Kind, Origin, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::scope::{git, git_identity_args};

/// launchd job label and plist name (`~/Library/LaunchAgents/<label>.plist`).
pub const LAUNCHD_LABEL: &str = "com.vncp.rematch.profile-sync";
/// Serializes syncs (session start vs the launchd job); older than this it is taken over.
const LOCK_FILE: &str = ".profile.lock";
const LOCK_STALE: Duration = Duration::from_secs(300);

#[derive(Subcommand, Debug, Clone)]
pub enum ProfileCmd {
    /// Create REMATCH_ROOT/profile (git init, or clone --remote), then sync it.
    Init(InitArgs),
    /// Move one skill dir or agent .md file into the profile, a link left in its place.
    Adopt {
        /// A skill directory (holding SKILL.md) or an agent file (`<name>.md`).
        path: PathBuf,
    },
    /// Commit local changes, pull --rebase, relink the views, push.
    Sync(SyncArgs),
    /// Branch, remote, ahead/behind (as of the last fetch), dirty paths, conflicts, machines.
    Status,
    /// Write ~/Library/LaunchAgents/com.vncp.rematch.profile-sync.plist (not loaded).
    InstallLaunchd(LaunchdArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Git remote to clone (an existing profile) or to push to (an empty one).
    #[arg(long)]
    pub remote: Option<String>,
    /// Move kept entries, hand-written `<agent_dir>/skills/*` and `<agent_dir>/agents/*.md`
    /// into the profile (hand-written skills become pinned entries).
    #[arg(long)]
    pub adopt: bool,
    /// Seconds for all network operations together.
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone)]
pub struct SyncArgs {
    /// Commit and pull, but do not push.
    #[arg(long)]
    pub no_push: bool,
    /// Seconds for all network operations together (fetch + push); past it the sync reports
    /// `offline: true` and goes on with the local state.
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone)]
pub struct LaunchdArgs {
    /// Seconds between syncs.
    #[arg(long, default_value_t = 300)]
    pub interval: u64,
}

pub fn cmd_profile(ctx: &AppCtx, cmd: ProfileCmd) -> Result<Value> {
    match cmd {
        ProfileCmd::Init(a) => init(ctx, &a),
        ProfileCmd::Adopt { path } => {
            require(ctx)?;
            let p = crate::app::absolute(&ctx.cwd.join(path))?;
            if let Some(why) = foreign_harness(ctx, &p) {
                bail!(
                    "{} is {why}: the profile holds omp skills and agents only (a dir with SKILL.md, an agent .md); \
                     CLAUDE.md, settings and other harnesses' files stay out. To adopt a skill for omp, copy it to {} first",
                    p.display(),
                    ctx.paths.skills_dir().join("<name>").display()
                );
            }
            let adopted = adopt_path(ctx, &p)?;
            let views = profile::link_views(&ctx.paths)?;
            library::sync_shared_skills(&ctx.ledger, &ctx.paths)?;
            let line = format!("adopted {} → {}", p.display(), adopted["to"].as_str().unwrap_or("?"));
            Ok(json!({"adopted": adopted, "views": views, "lines": [line]}))
        }
        ProfileCmd::Sync(a) => {
            require(ctx)?;
            sync(ctx, a.no_push, Duration::from_secs(a.timeout))
        }
        ProfileCmd::Status => status(ctx),
        ProfileCmd::InstallLaunchd(a) => install_launchd(ctx, a.interval),
    }
}

fn require(ctx: &AppCtx) -> Result<()> {
    if !profile::present(&ctx.paths) {
        bail!("no profile at {} (run `rematch profile init`)", profile::dir(&ctx.paths).display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// git

/// Network budget shared by the network commands of one invocation.
struct Budget {
    deadline: Instant,
}

impl Budget {
    fn new(timeout: Duration) -> Budget {
        Budget { deadline: Instant::now() + timeout }
    }

    /// What is left, at least one second so a late command still gets a chance.
    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1))
    }
}

/// A network git command (`fetch`, `push`, `clone`) bounded by the budget: `Err(reason)` when
/// it failed or ran out of time (never prompts for credentials: `GIT_TERMINAL_PROMPT=0`).
fn net(dir: &Path, args: &[&str], budget: &Budget) -> Result<(), String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0");
    let timeout = budget.left();
    match crate::proc::run_bounded(c, timeout) {
        Ok(b) if b.killed => Err(format!("git {} timed out after {} s", args[0], timeout.as_secs())),
        Ok(b) if b.code == Some(0) => Ok(()),
        Ok(b) => Err(format!("git {} failed: {}", args[0], b.stderr.trim())),
        Err(e) => Err(format!("git {}: {e:#}", args[0])),
    }
}

/// Current branch (unborn included), `main` when HEAD is detached.
fn current_branch(dir: &Path) -> String {
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "main".into())
}

fn has_head(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok()
}

fn origin(dir: &Path) -> Option<String> {
    git(dir, &["remote", "get-url", "origin"]).ok().filter(|s| !s.is_empty())
}

fn remote_ref(dir: &Path, branch: &str) -> Option<String> {
    let r = format!("refs/remotes/origin/{branch}");
    git(dir, &["rev-parse", "--verify", "-q", &r]).ok().map(|_| format!("origin/{branch}"))
}

/// Stage everything and commit `"<machine> <ts>"` when anything changed; the new head.
fn commit_all(dir: &Path, machine: &str) -> Result<Option<String>> {
    git(dir, &["add", "-A"])?;
    if git(dir, &["status", "--porcelain"])?.is_empty() {
        return Ok(None);
    }
    let msg = format!("{machine} {}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
    let mut args = git_identity_args(dir);
    args.extend(["commit".into(), "--no-verify".into(), "-q".into(), "-m".into(), msg]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(dir, &refs)?;
    git(dir, &["rev-parse", "HEAD"]).map(Some)
}

/// Rebase onto `upstream`; on a conflict the rebase is aborted (the tree is back at the local
/// commit, clean) and the conflicting paths are returned.
fn rebase(dir: &Path, upstream: &str) -> Result<Vec<String>> {
    let mut args = git_identity_args(dir);
    args.extend(["-c".into(), "rebase.autoStash=false".into(), "rebase".into(), "--quiet".into(), upstream.into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Err(err) = git(dir, &refs) else { return Ok(Vec::new()) };
    let mut conflicts: Vec<String> = git(dir, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect();
    if dir.join(".git").join("rebase-merge").exists() || dir.join(".git").join("rebase-apply").exists() {
        git(dir, &["rebase", "--abort"]).context("abort the rebase")?;
    }
    if conflicts.is_empty() {
        return Err(err);
    }
    conflicts.sort();
    conflicts.dedup();
    Ok(conflicts)
}

/// Exclusive per-root sync lock; removed on drop.
struct SyncLock(PathBuf);

impl SyncLock {
    fn take(root: &Path) -> Result<Option<SyncLock>> {
        let p = root.join(LOCK_FILE);
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
                Ok(_) => {
                    let _ = std::fs::write(&p, std::process::id().to_string());
                    return Ok(Some(SyncLock(p)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > LOCK_STALE);
                    if !stale {
                        return Ok(None);
                    }
                    let _ = std::fs::remove_file(&p);
                }
                Err(e) => return Err(e).with_context(|| format!("create {}", p.display())),
            }
        }
        Ok(None)
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// init / adopt

fn init(ctx: &AppCtx, a: &InitArgs) -> Result<Value> {
    let dir = profile::dir(&ctx.paths);
    let budget = Budget::new(Duration::from_secs(a.timeout));
    let created = !profile::present(&ctx.paths);
    let mut cloned = false;
    if created {
        if std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some()) {
            bail!("{} exists and is not a git repository; move it away first", dir.display());
        }
        std::fs::create_dir_all(&dir)?;
        match &a.remote {
            Some(url) => {
                let target = dir.to_string_lossy().to_string();
                // No line-ending conversion: profile files are hashed and compared byte for
                // byte across machines (Git for Windows defaults to `core.autocrlf=true`).
                if let Err(e) = net(&ctx.paths.root, &["clone", "--quiet", "-c", "core.autocrlf=false", url, &target], &budget) {
                    let _ = std::fs::remove_dir_all(&dir);
                    bail!("clone {url}: {e}");
                }
                cloned = true;
                if !has_head(&dir) {
                    // An empty remote, or one whose HEAD names a branch it lacks.
                    let branches = git(&dir, &["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin"])?;
                    let names: Vec<&str> = branches.lines().filter(|b| !b.is_empty() && *b != "HEAD").collect();
                    match ["main", "master"].into_iter().find(|b| names.contains(b)).or(names.first().copied()) {
                        Some(b) => {
                            git(&dir, &["checkout", "-q", "-B", b, &format!("origin/{b}")])?;
                        }
                        None => {
                            git(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
                        }
                    }
                }
            }
            None => {
                git(&dir, &["init", "-q"])?;
                git(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
            }
        }
    } else if let Some(url) = &a.remote {
        match origin(&dir) {
            None => {
                git(&dir, &["remote", "add", "origin", url])?;
            }
            Some(cur) if &cur != url => bail!("the profile's origin is {cur}, not {url} (change it with git remote set-url)"),
            Some(_) => {}
        }
    }
    git(&dir, &["config", "core.autocrlf", "false"])?;
    profile::ensure_layout(&ctx.paths)?;
    let adopted = if a.adopt { adopt_all(ctx)? } else { Vec::new() };
    let remaining = budget.deadline.saturating_duration_since(Instant::now());
    let synced = sync(ctx, false, remaining)?;
    let mut lines = vec![format!(
        "profile {} at {}{}",
        if created { if cloned { "cloned" } else { "created" } } else { "already present" },
        dir.display(),
        origin(&dir).map(|o| format!(" (origin {o})")).unwrap_or_default()
    )];
    lines.extend(adopted.iter().map(|x| format!("adopted {} → {}", x["from"].as_str().unwrap_or("?"), x["to"].as_str().unwrap_or("?"))));
    lines.extend(synced["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
    Ok(json!({"dir": dir, "created": created, "cloned": cloned, "remote": origin(&dir), "adopted": adopted, "sync": synced, "lines": lines}))
}

/// `--adopt`: kept entries (rematch-owned) first, then the hand-written skills and agents
/// left in the agent dir.
fn adopt_all(ctx: &AppCtx) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for s in [Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(s), ..Default::default() })? {
            let from = if e.kind == Kind::Agent { library::kept_agent_file(&ctx.paths, &e.name) } else { e.path.clone() };
            if let Some(n) = library::adopt_kept(&ctx.ledger, &ctx.paths, &e)? {
                let to = if n.kind == Kind::Agent { profile::agents_dir(&ctx.paths).join(format!("{}.md", n.name)) } else { n.path.clone() };
                out.push(json!({"name": n.name, "kind": n.kind, "id": n.id, "as": "kept", "from": from, "to": to}));
            }
        }
    }
    for (p, is_dir) in children(&ctx.paths.skills_dir())
        .into_iter()
        .map(|p| (p.clone(), true))
        .chain(children(&ctx.paths.agents_dir()).into_iter().map(|p| (p, false)))
    {
        let regular = std::fs::symlink_metadata(&p).is_ok_and(|m| if is_dir { m.is_dir() } else { m.is_file() });
        let wanted = if is_dir { p.join(SKILL_MD).is_file() } else { p.extension().is_some_and(|x| x == "md") };
        if !regular || !wanted || rematch_owned(ctx, &if is_dir { p.join(SKILL_MD) } else { p.clone() })? {
            continue;
        }
        out.push(adopt_path(ctx, &p)?);
    }
    Ok(out)
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// The file's `rematch:` block names an entry this ledger knows.
fn rematch_owned(ctx: &AppCtx, md: &Path) -> Result<bool> {
    let Some(b) = std::fs::read_to_string(md).ok().and_then(|t| SkillDoc::parse(&t).ok()).and_then(|d| d.rematch()) else {
        return Ok(false);
    };
    Ok(ctx.ledger.get_entry(&b.id)?.is_some())
}

/// What makes `p` another harness's file rather than an omp skill or agent (DESIGN §20: omp
/// only): under Claude Code's config dir (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) or Codex's
/// (`$CODEX_HOME`, else `~/.codex`; `~/.agents`), inside any `.claude`/`.codex`/`.agents`
/// dir, or a harness instruction or settings file (`CLAUDE.md`, `AGENTS.md`,
/// `settings.json`). `None` for anything under the omp agent dir.
fn foreign_harness(ctx: &AppCtx, p: &Path) -> Option<String> {
    let real = rematch_core::paths::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let agent = rematch_core::paths::canonicalize(&ctx.paths.agent_dir).unwrap_or_else(|_| ctx.paths.agent_dir.clone());
    if p.starts_with(&ctx.paths.agent_dir) || real.starts_with(&agent) {
        return None;
    }
    let home = rematch_core::paths::home_dir();
    let env_dir = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let roots = [
        ("Claude Code's", env_dir("CLAUDE_CONFIG_DIR").or_else(|| home.as_ref().map(|h| h.join(".claude")))),
        ("Codex's", env_dir("CODEX_HOME").or_else(|| home.as_ref().map(|h| h.join(".codex")))),
        ("Codex's", home.as_ref().map(|h| h.join(".agents"))),
    ];
    for (who, root) in roots {
        let Some(root) = root else { continue };
        let root_real = rematch_core::paths::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if p.starts_with(&root) || real.starts_with(&root_real) {
            return Some(format!("{who} ({})", root.display()));
        }
    }
    for c in real.components().chain(p.components()) {
        match c.as_os_str().to_str() {
            Some(".claude") => return Some("inside a Claude Code dir (.claude)".into()),
            Some(".codex" | ".agents") => return Some("inside a Codex dir (.codex, .agents)".into()),
            _ => {}
        }
    }
    match p.file_name().and_then(|n| n.to_str()) {
        Some("CLAUDE.md" | "CLAUDE.local.md") => Some("Claude Code's instructions file".into()),
        Some("AGENTS.md") => Some("a harness instructions file (AGENTS.md)".into()),
        Some("settings.json" | "settings.local.json") => Some("a harness settings file".into()),
        _ => None,
    }
}

/// Adopt one skill dir or agent file: a kept entry rematch owns moves with its ledger path;
/// anything else is moved as it is and linked back. A hand-written skill becomes a pinned
/// entry (measured, never moved or replaced by the loop); one registered before keeps its
/// entry with the new path.
fn adopt_path(ctx: &AppCtx, src: &Path) -> Result<Value> {
    let md = std::fs::symlink_metadata(src).with_context(|| format!("{} not found", src.display()))?;
    if md.file_type().is_symlink() {
        let t = std::fs::read_link(src)?;
        if t.starts_with(profile::dir(&ctx.paths)) {
            bail!("{} is already in the profile ({})", src.display(), t.display());
        }
        bail!("{} is a link to {}; adopt the target instead", src.display(), t.display());
    }
    let name = src.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("{} has no file name", src.display()))?.to_string();
    if md.is_file() {
        if !name.ends_with(".md") {
            bail!("{} is neither a skill directory nor an agent .md file", src.display());
        }
        library::validate_name(name.trim_end_matches(".md"))?;
        let dst = profile::agents_dir(&ctx.paths).join(&name);
        if dst.exists() {
            // The same agent already adopted from another machine: identical content means
            // this copy is redundant — replace it with the link. Different content is a
            // conflict for the user to resolve, never silently overwritten.
            if same_contents(src, &dst)? {
                std::fs::remove_file(src)?;
                library::symlink_dir(&dst, src)?;
                return Ok(json!({"name": name.trim_end_matches(".md"), "kind": "agent", "as": "linked (identical copy already in the profile)", "from": src, "to": dst}));
            }
            bail!("the profile already holds a different agents/{name}; diff it against {} and adopt or discard by hand", src.display());
        }
        profile::move_and_link(src, &dst)?;
        return Ok(json!({"name": name.trim_end_matches(".md"), "kind": "agent", "as": "file", "from": src, "to": dst}));
    }
    if !src.join(SKILL_MD).is_file() {
        bail!("{} holds no SKILL.md", src.display());
    }
    library::validate_name(&name)?;
    let named = ctx.ledger.list_entries(&EntryFilter { name: Some(name.clone()), ..Default::default() })?;
    let same = |p: &Path| p == src || rematch_core::paths::canonicalize(p).is_ok_and(|c| c == src);
    let existing = named.into_iter().rev().find(|e| same(&e.path) && !matches!(e.status, Status::Retired | Status::Dormant | Status::Stale));
    if let Some(e) = &existing {
        if let Some(n) = library::adopt_kept(&ctx.ledger, &ctx.paths, e)? {
            return Ok(json!({"name": n.name, "kind": n.kind, "id": n.id, "as": "kept", "from": src, "to": n.path}));
        }
    }
    let dst = profile::skills_dir(&ctx.paths).join(&name);
    if dst.exists() {
        if same_tree(src, &dst)? {
            std::fs::remove_dir_all(src)?;
            library::symlink_dir(&dst, src)?;
            return Ok(json!({"name": name, "kind": "skill", "as": "linked (identical copy already in the profile)", "from": src, "to": dst}));
        }
        bail!("the profile already holds a different skills/{name}; diff it against {} and adopt or discard by hand", src.display());
    }
    profile::move_and_link(src, &dst)?;
    let e = match existing {
        Some(mut e) => {
            e.path = dst.clone();
            e.updated_at = ctx.now;
            ctx.ledger.upsert_entry(&e)?;
            e
        }
        None => {
            let (mut e, _) = crate::improve::register(ctx, &name, &dst, &[], SHARED_BRANCH)?;
            e.status = Status::Pinned;
            e.pinned = true;
            e.origin = Origin::Import { source: "profile".into() };
            e.status_note = Some("adopted into the profile".into());
            e.status_since = Some(ctx.now);
            e.updated_at = ctx.now;
            ctx.ledger.upsert_entry(&e)?;
            e
        }
    };
    Ok(json!({"name": name, "kind": "skill", "id": e.id, "as": if e.pinned { "pinned" } else { "registered" }, "from": src, "to": dst}))
}

// ---------------------------------------------------------------------------------------------
// sync

/// `fragments/<name>.md` ← the `AGENTS.fragment.md` of every kept/pinned entry whose dir is not
/// already in the profile (kept agents); a fragment whose entries here all left kept/pinned
/// and whose text is still theirs goes.
fn collect_fragments(ctx: &AppCtx) -> Result<Vec<String>> {
    let dir = profile::fragments_dir(&ctx.paths);
    std::fs::create_dir_all(&dir)?;
    let inside = profile::dir(&ctx.paths);
    let mut changed = Vec::new();
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    let live = |e: &&rematch_core::types::Entry| matches!(e.status, Status::Kept | Status::Pinned) || e.pinned;
    for e in all.iter().filter(live) {
        if e.path.starts_with(&inside) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path.join(AGENTS_FRAGMENT)) else { continue };
        let f = dir.join(format!("{}.md", e.name));
        if std::fs::read_to_string(&f).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&f, &text)?;
            changed.push(format!("fragments/{}.md", e.name));
        }
    }
    for f in children(&dir) {
        let Some(name) = f.file_stem().and_then(|n| n.to_str()) else { continue };
        let named: Vec<_> = all.iter().filter(|e| e.name == name).collect();
        if named.is_empty() || named.iter().any(live) {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        if named.iter().any(|e| std::fs::read_to_string(e.path.join(AGENTS_FRAGMENT)).ok().as_deref() == Some(text.as_str())) {
            std::fs::remove_file(&f)?;
            changed.push(format!("fragments/{name}.md (removed)"));
        }
    }
    Ok(changed)
}

/// One sync: collect local files into the profile, commit, fetch + rebase onto the remote
/// (conflicts abort the rebase and are reported), relink the views, push. Network failures
/// and timeouts report `offline: true` and never fail the command.
fn sync(ctx: &AppCtx, no_push: bool, timeout: Duration) -> Result<Value> {
    let Some(_lock) = SyncLock::take(&ctx.paths.root)? else {
        return Ok(json!({"skipped": "another profile sync is running", "lines": ["profile sync skipped: another sync is running"]}));
    };
    let started = Instant::now();
    let dir = profile::dir(&ctx.paths);
    let machine = machine_name();
    profile::ensure_layout(&ctx.paths)?;
    let collected = profile::collect(&ctx.paths)?;
    let fragments = collect_fragments(ctx)?;
    // The approved personal instruction texts (`instructions/<name>.md`), canonical across machines.
    let instructions = rematch_core::instructions::collect_profile(&ctx.ledger, &ctx.paths)?;
    let exposed_before = rematch_core::instructions::exposure(&ctx.ledger, &ctx.paths)?;
    profile::note_machine(&ctx.paths, &machine)?;
    let committed = commit_all(&dir, &machine)?;
    let branch = current_branch(&dir);
    let remote = origin(&dir);
    // The budget covers the network commands only (fetch, push).
    let budget = Budget::new(timeout);
    let (mut offline, mut error, mut conflicts, mut pulled, mut pushed) = (false, None::<String>, Vec::new(), false, false);
    // Without a remote there is nothing to conflict with; with one, only a fetch tells.
    let mut rebased = remote.is_none();
    if remote.is_some() {
        match net(&dir, &["fetch", "--quiet", "origin"], &budget) {
            Err(e) => {
                offline = true;
                error = Some(e);
            }
            Ok(()) => {
                rebased = true;
                if let Some(up) = remote_ref(&dir, &branch) {
                    let before = git(&dir, &["rev-parse", "HEAD"]).ok();
                    if before.is_none() {
                        git(&dir, &["reset", "-q", "--hard", &up])?;
                    } else {
                        conflicts = rebase(&dir, &up)?;
                    }
                    pulled = conflicts.is_empty() && git(&dir, &["rev-parse", "HEAD"]).ok() != before;
                }
            }
        }
    }
    let views = profile::link_views(&ctx.paths)?;
    // Applies the pulled `instructions/` to this machine's personal AGENTS.md too.
    let skills = library::sync_shared_skills(&ctx.ledger, &ctx.paths)?;
    let exposed_after = rematch_core::instructions::exposure(&ctx.ledger, &ctx.paths)?;
    profile::write_index(&ctx.paths, &index_skills(&ctx.paths))?;
    if remote.is_some() && !offline && conflicts.is_empty() && !no_push && has_head(&dir) {
        let ahead = match remote_ref(&dir, &branch) {
            Some(up) => git(&dir, &["rev-list", "--count", &format!("{up}..HEAD")])?.parse::<u64>().unwrap_or(0),
            None => 1,
        };
        if ahead > 0 {
            match net(&dir, &["push", "--quiet", "origin", &format!("HEAD:refs/heads/{branch}")], &budget) {
                Ok(()) => {
                    pushed = true;
                    // Keep the local remote-tracking ref in step for `status`.
                    let _ = git(&dir, &["update-ref", &format!("refs/remotes/origin/{branch}"), "HEAD"]);
                }
                Err(e) => {
                    offline = true;
                    error = Some(e);
                }
            }
        }
    }
    // Offline, nothing was rebased: the last known conflicts stand.
    if rebased {
        profile::write_conflicts(&ctx.paths, &conflicts)?;
    }
    let mut lines = Vec::new();
    if let Some(c) = &committed {
        lines.push(format!("committed {} as {machine}", &c[..c.len().min(8)]));
    }
    if pulled {
        lines.push(format!("pulled origin/{branch}"));
    }
    if pushed {
        lines.push(format!("pushed {branch}"));
    }
    if !conflicts.is_empty() {
        lines.push(conflict_line(&dir, &conflicts));
    }
    if offline {
        lines.push(format!("offline: {}", error.as_deref().unwrap_or("remote unreachable")));
    }
    for v in &views.untouched {
        lines.push(format!("not linked (a local file holds the name): {v}"));
    }
    for v in &views.scoped {
        lines.push(format!("withdrawn from <agent_dir>/{v}: the name has a place scope (policy.json scopes); sessions already running keep what they loaded"));
    }
    for v in &collected.kept_local {
        lines.push(format!("kept local (the profile has its own): {v}"));
    }
    let names = |x: &rematch_core::instructions::Exposure| x.exposed.iter().map(|e| (e.name.clone(), e.sha.clone())).collect::<std::collections::BTreeSet<_>>();
    let (was, now) = (names(&exposed_before), names(&exposed_after));
    let applied: Vec<&str> = now.difference(&was).map(|(n, _)| n.as_str()).collect();
    let removed: Vec<&str> = was.difference(&now).map(|(n, _)| n.as_str()).collect();
    if !applied.is_empty() || !removed.is_empty() {
        lines.push(format!(
            "personal AGENTS.md {}: {}{}{}; new sessions load it, a running session keeps the text it was already sent",
            exposed_after.target.display(),
            if applied.is_empty() { String::new() } else { format!("applied {}", applied.join(", ")) },
            if applied.is_empty() || removed.is_empty() { "" } else { "; " },
            if removed.is_empty() { String::new() } else { format!("removed {}", removed.join(", ")) },
        ));
    }
    let gone: Vec<&str> = instructions.iter().filter_map(|l| l.strip_suffix(" (withdrawn profile-wide)")).collect();
    if !gone.is_empty() {
        lines.push(format!(
            "profile: withdrew {} (declined or withdrawn here) — profile-wide: every machine and omp profile syncing it drops the text at its next sync, its own approval included, until someone approves those bytes again",
            gone.join(", ")
        ));
    }
    for c in exposed_after.conflicts.iter().filter(|c| *c != "*") {
        lines.push(format!("personal AGENTS.md: block {c} was edited by you and left as is (`rematch library inspect instructions-conflict:{c}`)"));
    }
    let plan = rematch_core::instructions::plan_sync(&ctx.ledger, &ctx.paths)?;
    if !plan.over_budget.is_empty() {
        lines.push(format!(
            "personal AGENTS.md {}: over the always-on budget, not added: {} (blocked `instructions-applied:<name>` actions in `rematch status`)",
            plan.target.display(),
            plan.over_budget.join(", ")
        ));
    }
    if rematch_core::instructions::skipped_profile(&ctx.paths) {
        lines.push(format!(
            "omp profile {} is in policy.json skip_profiles: the profile's instructions are not applied to {}, and its approvals stay out of the profile",
            ctx.paths.profile.as_deref().unwrap_or("?"),
            plan.target.display()
        ));
    }
    lines.extend(crate::library_cmd::drift_lines(ctx));
    Ok(json!({
        "machine": machine,
        "branch": branch,
        "remote": remote,
        "committed": committed,
        "pulled": pulled,
        "pushed": pushed,
        "offline": offline,
        "error": error,
        "conflicts": conflicts,
        "collected": collected,
        "fragments": fragments,
        "instructions": {"collected": instructions, "before": exposed_before, "after": exposed_after},
        "views": views,
        "skills": {"added": skills.added, "pruned": skills.pruned, "total": skills.total},
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "lines": lines,
    }))
}

/// The digest's line for the conflicts the last sync left (DESIGN §20).
pub fn conflict_line(dir: &Path, conflicts: &[String]) -> String {
    format!(
        "profile: {} conflict(s) not synced ({}); resolve with `git -C {} pull --rebase`, then `rematch profile sync`",
        conflicts.len(),
        conflicts.join(", "),
        dir.display()
    )
}

/// The origin of every kept source (the last commit touching `skills/<name>`), for
/// `profile/.index.json`.
fn index_skills(paths: &rematch_core::paths::RematchPaths) -> profile::ProfileIndex {
    let dir = profile::dir(paths);
    let skills = profile::kept_sources(paths).into_iter().filter_map(|(name, _)| Some((name.clone(), origin_of(&dir, &name)?))).collect();
    profile::ProfileIndex { skills }
}

/// `git log -1 --format=%s -- skills/<name>`: the commit subject `"<machine> <ts>"`.
fn origin_of(dir: &Path, name: &str) -> Option<profile::SkillOrigin> {
    let out = git(dir, &["log", "-1", "--format=%H%x1f%s", "--", &format!("skills/{name}")]).ok()?;
    let (commit, subject) = out.trim().split_once('\u{1f}')?;
    Some(profile::SkillOrigin::from_subject(subject, commit))
}

/// A skill in `profile/skills/` that no entry of this ledger holds (kept or adopted on
/// another machine): it loads here (`skills sync`, `match`) and is never counted here.
pub struct ProfileKept {
    pub name: String,
    pub dir: PathBuf,
    /// The machine of the last commit that touched it (`.index.json`, else `git log`).
    pub from: Option<String>,
}

impl ProfileKept {
    pub fn label(&self) -> String {
        format!("kept (profile, from {})", self.from.as_deref().unwrap_or("unknown"))
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "status": "kept", "source": "profile", "from": self.from, "path": self.dir, "counted": false})
    }
}

/// The profile's kept sources without a ledger entry on this machine (none without a profile).
pub fn profile_kept(ctx: &AppCtx) -> Result<Vec<ProfileKept>> {
    let sources = profile::kept_sources(&ctx.paths);
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let canon = |p: &Path| rematch_core::paths::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let held: std::collections::BTreeSet<PathBuf> =
        ctx.ledger.list_entries(&EntryFilter::default())?.iter().flat_map(|e| [e.path.clone(), canon(&e.path)]).collect();
    let index = profile::load_index(&ctx.paths);
    let dir = profile::dir(&ctx.paths);
    Ok(sources
        .into_iter()
        .filter(|(_, p)| !held.contains(p) && !held.contains(&canon(p)))
        .map(|(name, p)| {
            let from = index.skills.get(&name).map(|o| o.machine.clone()).or_else(|| origin_of(&dir, &name).map(|o| o.machine));
            ProfileKept { name, dir: p, from }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// status / launchd

pub fn status(ctx: &AppCtx) -> Result<Value> {
    let dir = profile::dir(&ctx.paths);
    let machine = machine_name();
    if !profile::present(&ctx.paths) {
        return Ok(json!({"present": false, "dir": dir, "machine": machine, "lines": ["no profile (rematch profile init [--remote <url>] [--adopt])"]}));
    }
    let branch = current_branch(&dir);
    let remote = origin(&dir);
    let head = git(&dir, &["rev-parse", "--short", "HEAD"]).ok();
    let (ahead, behind) = match remote_ref(&dir, &branch).filter(|_| head.is_some()) {
        Some(up) => {
            let counts = git(&dir, &["rev-list", "--left-right", "--count", &format!("HEAD...{up}")])?;
            let mut it = counts.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
            (it.next(), it.next())
        }
        None => (None, None),
    };
    let dirty: Vec<String> = git(&dir, &["status", "--porcelain"])?.lines().map(str::to_string).collect();
    let conflicts = profile::read_conflicts(&ctx.paths);
    let pj = profile::load_profile_json(&ctx.paths)?;
    let count = |d: PathBuf, ext: Option<&str>| children(&d).into_iter().filter(|p| ext.is_none_or(|x| p.extension().is_some_and(|e| e == x))).count();
    let mut roles = serde_json::Map::new();
    for role in [crate::config::ROLE_EDITOR, crate::config::ROLE_CLASSIFIER, crate::config::ROLE_SOLVER] {
        if let Some(sel) = profile::role_selector(&ctx.paths, role, &machine)? {
            roles.insert(role.into(), json!(sel));
        }
    }
    let skills = count(profile::skills_dir(&ctx.paths), None);
    let agents = count(profile::agents_dir(&ctx.paths), Some("md"));
    let hooks = count(profile::hooks_dir(&ctx.paths), Some("ts"));
    let instructions = count(profile::instructions_dir(&ctx.paths), Some("md"));
    let mut lines = vec![
        format!(
            "profile {} on {branch} at {}{}",
            dir.display(),
            head.as_deref().unwrap_or("(no commit)"),
            remote.as_deref().map(|r| format!(", origin {r}")).unwrap_or_else(|| ", no remote".into())
        ),
        format!("{skills} skill(s), {agents} agent(s), {hooks} hook(s), {instructions} personal instruction text(s); machines: {}", pj.machines.join(", ")),
    ];
    if let (Some(a), Some(b)) = (ahead, behind) {
        lines.push(format!("{a} ahead, {b} behind origin/{branch} (as of the last fetch)"));
    }
    if !dirty.is_empty() {
        lines.push(format!("{} uncommitted change(s)", dirty.len()));
    }
    if !conflicts.is_empty() {
        lines.push(conflict_line(&dir, &conflicts));
    }
    Ok(json!({
        "present": true, "dir": dir, "machine": machine, "branch": branch, "remote": remote, "head": head,
        "ahead": ahead, "behind": behind, "dirty": dirty, "conflicts": conflicts, "machines": pj.machines,
        "skills": skills, "agents": agents, "hooks": hooks, "instructions": instructions, "roles": roles, "lines": lines,
    }))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The binary launchd runs: the `~/.local/bin/rematch` shim `install.sh` writes when it exists
/// (stable across rebuilds), else this executable.
fn launchd_binary() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let shim = PathBuf::from(home).join(".local/bin/rematch");
        if shim.is_file() {
            return Ok(shim);
        }
    }
    std::env::current_exe().context("locate the rematch binary")
}

/// The launchd job: `rematch profile sync` (pull and push) every `interval` seconds and at
/// load, with the environment that picks this root and agent dir.
pub fn launchd_plist(bin: &Path, interval: u64, log: &Path, env: &[(String, String)]) -> String {
    let vars: String = env.iter().map(|(k, v)| format!("\t\t<key>{}</key>\n\t\t<string>{}</string>\n", xml(k), xml(v))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{LAUNCHD_LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
\t\t<string>{bin}</string>\n\
\t\t<string>profile</string>\n\
\t\t<string>sync</string>\n\
\t</array>\n\
\t<key>StartInterval</key>\n\
\t<integer>{interval}</integer>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>ProcessType</key>\n\
\t<string>Background</string>\n\
\t<key>EnvironmentVariables</key>\n\
\t<dict>\n\
{vars}\t</dict>\n\
\t<key>StandardOutPath</key>\n\
\t<string>{log}</string>\n\
\t<key>StandardErrorPath</key>\n\
\t<string>{log}</string>\n\
</dict>\n\
</plist>\n",
        bin = xml(&bin.to_string_lossy()),
        log = xml(&log.to_string_lossy()),
    )
}

/// `~/Library/LaunchAgents/<label>.plist`.
pub fn launchd_plist_path() -> Option<PathBuf> {
    rematch_core::paths::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist")))
}

fn install_launchd(ctx: &AppCtx, interval: u64) -> Result<Value> {
    if interval == 0 {
        bail!("--interval must be at least 1 second");
    }
    let plist = launchd_plist_path().ok_or_else(|| anyhow!("HOME is not set"))?;
    let home = rematch_core::paths::home_dir().ok_or_else(|| anyhow!("HOME is not set"))?;
    let bin = launchd_binary()?;
    let log = ctx.paths.root.join("profile-sync.log");
    // launchd starts with a bare environment: carry what picks git, the root and the agent dir.
    let mut env = vec![("HOME".to_string(), home.to_string_lossy().to_string())];
    // launchd provides SSH_AUTH_SOCK itself; a socket path from this login would go stale.
    for k in ["PATH", "REMATCH_ROOT", "PI_CONFIG_DIR", "PI_CODING_AGENT_DIR", "OMP_PROFILE", "PI_PROFILE", "REMATCH_MACHINE"] {
        if let Some(v) = std::env::var_os(k).filter(|v| !v.is_empty()) {
            env.push((k.to_string(), v.to_string_lossy().to_string()));
        }
    }
    let text = launchd_plist(&bin, interval, &log, &env);
    std::fs::create_dir_all(plist.parent().unwrap_or(&home))?;
    std::fs::write(&plist, &text).with_context(|| format!("write {}", plist.display()))?;
    let load = format!("launchctl load -w {}", plist.display());
    let lines = vec![
        format!("wrote {} (every {interval} s: {} profile sync; log {})", plist.display(), bin.display(), log.display()),
        format!("load it with: {load}"),
        format!("stop it with: launchctl unload -w {}", plist.display()),
    ];
    Ok(json!({"plist": plist, "binary": bin, "interval": interval, "log": log, "load": load, "loaded": false, "lines": lines}))
}

/// Byte-equal regular files.
fn same_contents(a: &Path, b: &Path) -> Result<bool> {
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Same relative file set with byte-equal contents (symlinks compared by target).
fn same_tree(a: &Path, b: &Path) -> Result<bool> {
    fn listing(root: &Path) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
        let mut out = std::collections::BTreeMap::new();
        fn walk(root: &Path, cur: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) -> Result<()> {
            for ent in std::fs::read_dir(cur)? {
                let p = ent?.path();
                let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().to_string();
                if rel == "rematch.json" || rel == "EXPERIENCE.md" {
                    continue;
                }
                let md = std::fs::symlink_metadata(&p)?;
                if md.is_dir() {
                    walk(root, &p, out)?;
                } else if md.file_type().is_symlink() {
                    out.insert(rel, std::fs::read_link(&p)?.to_string_lossy().into_owned().into_bytes());
                } else {
                    out.insert(rel, std::fs::read(&p)?);
                }
            }
            Ok(())
        }
        walk(root, root, &mut out)?;
        Ok(out)
    }
    Ok(listing(a)? == listing(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_and_runs_a_full_sync() {
        let p = launchd_plist(Path::new("/u/a&b/rematch"), 300, Path::new("/r/profile-sync.log"), &[("PATH".into(), "/usr/bin:/bin".into())]);
        assert!(p.contains("<string>/u/a&amp;b/rematch</string>\n\t\t<string>profile</string>\n\t\t<string>sync</string>\n\t</array>"), "{p}");
        assert!(p.contains("<integer>300</integer>") && p.contains("<key>PATH</key>") && !p.contains("--no-push"), "{p}");
    }
}
