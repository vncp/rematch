//! `ship --scope team|fork` (SPEC §Scope and who merges): entries beyond personal
//! scope leave as a git branch in the target repo, never merged by the binary.
//!
//! - team: kept/provisional entries whose predicate recurred across ≥ 2 repos (or that
//!   are listed with `--entry`) are written to `<repo>/.omp/skills/<name>/` (agents:
//!   `<repo>/.omp/agents/<name>.md`) on branch `rematch/<name>-<short id>`, committed with
//!   an evidence body. An `AGENTS.fragment.md` in the entry becomes the section
//!   `## rematch: <name>` of `<repo>/AGENTS.md` (replaced in place when present) and is
//!   not copied into the skill dir: the team PR is the only way text reaches AGENTS.md.
//! - fork: entries with `fork_path` (`library set-fork-path`) are written to
//!   `<repo>/<fork_path>` (`--repo`, else the entry's `fork_repo`); the body adds the
//!   built fingerprint (`omp --version`, sha256 of the file) and before/after numbers
//!   per family.
//!
//! The branch is built in a temporary worktree from the repo's current HEAD, so the
//! user's checkout is untouched. `--push` pushes it; `--pr` (requires `--push` and
//! `gh`) opens a PR. Without them nothing touches the network.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use rematch_core::counters::{harmed, recurred};
use rematch_core::experience::{parse_experience, EXPERIENCE_MD};
use rematch_core::ledger::{enum_str, sha256_hex, EntryFilter, SessionFilter};
use rematch_core::library::{load_entry, validate_fork_path, AGENTS_FRAGMENT, AGENT_MD, SKILL_MD};
use rematch_core::lint::{lint_entry, LintContext};
use rematch_core::package::{RematchBlock, SkillDoc};
use rematch_core::predicate::{count_hit, count_match, predicate_hash, Place};
use rematch_core::{Activation, Counters, Entry, Family, FamilyCounters, Kind, Receipt, Status};
use serde::Serialize;
use serde_json::{json, Value};

use super::{git, git_commit, lint_config, lint_errors, origin_repo, package_files, resolve_entry, save_entry, short_id};
use crate::app::{absolute, AppCtx};
use crate::envelope::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShipScope {
    Team,
    Fork,
}

#[derive(Debug, Clone, Args)]
pub struct ShipArgs {
    /// team (`<repo>/.omp/skills/`, `.omp/agents/`, `AGENTS.md`) or fork (`<repo>/<fork_path>`).
    #[arg(long, value_enum)]
    pub scope: ShipScope,
    /// Target repo (team: defaults to the entry's originating repo; fork: defaults to the
    /// entry's `fork_repo`).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Entry id or name (repeatable). Listing an entry makes it eligible for team scope.
    #[arg(long = "entry")]
    pub entries: Vec<String>,
    /// Push the branch to `--remote` (default: no network).
    #[arg(long)]
    pub push: bool,
    /// Open a PR with `gh pr create --fill --body-file` (requires --push; never merges).
    #[arg(long)]
    pub pr: bool,
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Serialize)]
struct Shipped {
    entry: String,
    name: String,
    repo: PathBuf,
    branch: String,
    commit: String,
    files: Vec<String>,
    pushed: bool,
    pr_url: Option<String>,
    receipt: String,
}

#[derive(Debug, Serialize)]
struct Skipped {
    entry: String,
    name: String,
    reason: String,
}

/// Per-family counters as numbers for the PR body.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Numbers {
    pub matched: u32,
    pub recurrence: u32,
    pub harm: u32,
    pub per_family: BTreeMap<Family, FamilyCounters>,
}

impl Numbers {
    fn from_counters(c: &Counters) -> Numbers {
        Numbers { matched: c.matched, recurrence: c.recurrence, harm: c.harm, per_family: c.per_family.clone() }
    }

    fn render(&self) -> String {
        let mut s = format!("matched {} · recurrence {} · harm {}", self.matched, self.recurrence, self.harm);
        for (f, c) in &self.per_family {
            s.push_str(&format!("\n  - {}: matched {}, recurrence {}, harm {}", f.0, c.matched, c.recurrence, c.harm));
        }
        s
    }
}

pub fn cmd_ship(ctx: &AppCtx, args: ShipArgs) -> Result<Value> {
    if args.pr && !args.push {
        return Err(Usage("--pr needs --push (the PR head must exist on the remote); nothing was changed".into()).into());
    }
    if args.pr && crate::omp::which("gh").is_none() {
        bail!("--pr needs the GitHub CLI `gh` on PATH");
    }
    let repo_arg = args.repo.as_deref().map(absolute).transpose()?;
    let explicit = !args.entries.is_empty();
    let (candidates, mut skipped) = select(ctx, &args)?;
    let mut shipped = Vec::new();
    for e in candidates {
        let target = match (&repo_arg, args.scope) {
            (Some(r), _) => r.clone(),
            (None, ShipScope::Fork) => match &e.fork_repo {
                Some(r) => r.clone(),
                None => {
                    skipped.push(skip(&e, "no --repo and the entry has no fork_repo (`library set-fork-path <id> <path> --repo <fork>`)"));
                    continue;
                }
            },
            (None, ShipScope::Team) => match origin_repo(ctx, &e)? {
                Some(r) => r,
                None => {
                    skipped.push(skip(&e, "no --repo and the entry has no originating repo in the ledger"));
                    continue;
                }
            },
        };
        let res = match args.scope {
            ShipScope::Team => ship_team(ctx, &e, &target, &args),
            ShipScope::Fork => ship_fork(ctx, &e, &target, &args),
        };
        match res {
            Ok(s) => shipped.push(s),
            Err(err) => skipped.push(skip(&e, &format!("{err:#}"))),
        }
    }
    if explicit && shipped.is_empty() {
        let why: Vec<String> = skipped.iter().map(|s| format!("{}: {}", s.name, s.reason)).collect();
        bail!("nothing shipped: {}", why.join("; "));
    }
    Ok(json!({ "scope": args.scope, "shipped": shipped, "skipped": skipped }))
}

fn skip(e: &Entry, reason: &str) -> Skipped {
    Skipped { entry: e.id.clone(), name: e.name.clone(), reason: reason.to_string() }
}

/// Distinct repos of ledger sessions whose content matches the entry's predicate
/// (stems/exts only: the repo glob names one repo by construction).
pub fn recurrence_repos(ctx: &AppCtx, e: &Entry) -> Result<BTreeSet<String>> {
    Ok(ctx
        .ledger
        .list_sessions(&SessionFilter::default())?
        .into_iter()
        .filter(|s| count_hit(&e.predicate, &s.exts, &s.stems))
        .filter_map(|s| s.repo)
        .collect())
}

/// `fork_path` is set by `library set-fork-path` (ledger + rematch.json) or by the
/// editor; a hand edit of the entry's `rematch.json` is adopted into the ledger too.
fn adopt_fork_path(ctx: &AppCtx, mut e: Entry) -> Result<Entry> {
    if e.fork_path.is_none() {
        if let Some(fp) = load_entry(&e.path).ok().and_then(|d| d.fork_path) {
            e.fork_path = Some(fp);
            ctx.ledger.upsert_entry(&e)?;
        }
    }
    Ok(e)
}

fn select(ctx: &AppCtx, args: &ShipArgs) -> Result<(Vec<Entry>, Vec<Skipped>)> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let live = |e: &Entry| matches!(e.status, Status::Kept | Status::Provisional | Status::Pinned);
    if !args.entries.is_empty() {
        for r in &args.entries {
            let e = resolve_entry(ctx, r)?;
            let e = adopt_fork_path(ctx, e)?;
            if !live(&e) {
                skipped.push(skip(&e, &format!("status {} (only kept or provisional entries ship)", enum_str(&e.status))));
            } else if args.scope == ShipScope::Fork && e.fork_path.is_none() {
                skipped.push(skip(&e, "no fork_path (set one with `rematch library set-fork-path <id> <repo-relative path>`; fork scope ships per-model prompt/tool/compaction files)"));
            } else {
                out.push(e);
            }
        }
        return Ok((out, skipped));
    }
    // Built-ins ship with the binary, not to a repo: never selected automatically.
    for e in ctx.ledger.list_entries(&EntryFilter::default())?.into_iter().filter(|e| live(e) && !rematch_core::library::is_builtin(e)) {
        let e = adopt_fork_path(ctx, e)?;
        match args.scope {
            ShipScope::Team => {
                if recurrence_repos(ctx, &e)?.len() >= 2 {
                    out.push(e);
                }
            }
            ShipScope::Fork => {
                if e.fork_path.is_some() {
                    out.push(e);
                }
            }
        }
    }
    Ok((out, skipped))
}

/// A temporary worktree on a new branch; removed on drop (the branch stays).
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Worktree {
    fn create(repo: &Path, branch: &str) -> Result<Worktree> {
        let top = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"]).context("target is not a git repo")?);
        if git(&top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
            bail!("branch {branch} already exists in {} (already shipped)", top.display());
        }
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("wt");
        git(&top, &["worktree", "add", "-q", "-b", branch, &path.to_string_lossy(), "HEAD"])?;
        Ok(Worktree { repo: top, path, _tmp: tmp })
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = git(&self.repo, &["worktree", "remove", "--force", &self.path.to_string_lossy()]);
        let _ = git(&self.repo, &["worktree", "prune"]);
    }
}

fn branch_name(e: &Entry) -> String {
    format!("rematch/{}-{}", e.name, short_id(&e.id))
}

fn lint_before_ship(ctx: &AppCtx, e: &Entry) -> Result<()> {
    let rep = lint_entry(&e.path, e, &lint_config(ctx)?, &LintContext::default())?;
    if !rep.ok {
        bail!("lint failed: {}", lint_errors(&rep));
    }
    Ok(())
}

#[cfg(unix)]
fn non_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

#[cfg(not(unix))]
fn non_executable(_p: &Path) -> Result<()> {
    Ok(())
}

fn experience_excerpt(e: &Entry) -> Option<String> {
    let text = std::fs::read_to_string(e.path.join(EXPERIENCE_MD)).ok()?;
    let last = parse_experience(&text, &e.path).pop()?;
    Some(format!(
        "{} [{}]\nSituation: {}\nLesson: {}\nCounters: {}",
        last.written_at,
        enum_str(&last.tag),
        last.situation.trim(),
        last.lesson.trim(),
        last.counters.trim()
    ))
}

/// Shared evidence body: counters, matched sessions, families, redirect class,
/// predicted fix/regression, EXPERIENCE.md excerpt.
fn evidence_body(ctx: &AppCtx, e: &Entry) -> Result<String> {
    let c = ctx.ledger.counters(&e.id)?;
    let sessions = ctx.ledger.counter_rows(&e.id)?.len();
    let repos = recurrence_repos(ctx, e)?;
    let mut families: BTreeSet<String> = c.per_family.keys().map(|f| f.0.clone()).collect();
    families.insert(e.family.0.clone());
    families.extend(e.verified_on.iter().map(|f| f.0.clone()));
    let mut b = String::new();
    b.push_str(&format!("Entry: {} ({}, {} v{}, branch {})\n", e.id, enum_str(&e.kind), enum_str(&e.status), e.version, e.branch));
    b.push_str(&format!("Trigger: {}\n\n", e.trigger_line.trim()));
    b.push_str(&format!("Counters: {}\n", Numbers::from_counters(&c).render()));
    b.push_str(&format!("Tokens {} · turns {}\n", c.tokens, c.turns));
    b.push_str(&format!("Matched sessions: {sessions}\n"));
    b.push_str(&format!("Repos where the predicate recurred: {}\n", repos.len()));
    b.push_str(&format!("Families: created under {}; seen {}\n", e.family.0, families.into_iter().collect::<Vec<_>>().join(", ")));
    if !e.verified_on.is_empty() {
        b.push_str(&format!("Verified on: {}\n", e.verified_on.iter().map(|f| f.0.as_str()).collect::<Vec<_>>().join(", ")));
    }
    b.push_str(&format!(
        "Redirect class: {}\n",
        e.redirect_class.map(|r| enum_str(&r)).unwrap_or_else(|| "none (error cluster or takeover)".into())
    ));
    if let Some(m) = &e.manifest {
        b.push_str(&format!("Predicted fix: {}\nPredicted regression: {}\n", m.predicted_fix.trim(), m.predicted_regression.trim()));
    }
    if let Some(x) = experience_excerpt(e) {
        b.push_str(&format!("\nEXPERIENCE.md (latest record):\n{x}\n"));
    }
    Ok(b)
}

fn file_hashes(root: &Path, rels: &[String]) -> Result<Vec<(String, String)>> {
    rels.iter()
        .map(|r| Ok((r.clone(), sha256_hex(&std::fs::read(root.join(r)).with_context(|| format!("read {r}"))?))))
        .collect()
}

fn record_receipt(ctx: &AppCtx, e: &Entry, what: &str, repo: &Path, branch: &str, commit: &str, files: &[(String, String)]) -> Result<String> {
    let cert = json!({
        "entry": e.id, "what": what, "repo": repo, "branch": branch, "commit": commit,
        "files": files, "at": ctx.now,
    });
    let certificate = ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
    let row = ctx.ledger.insert_receipt(&Receipt { certificate, entry: e.id.clone(), activation: Activation::Pr })?;
    Ok(row.id)
}

/// Push and optionally open the PR; returns the PR URL when one was created.
fn publish(repo: &Path, branch: &str, body: &str, args: &ShipArgs) -> Result<(bool, Option<String>)> {
    if !args.push {
        return Ok((false, None));
    }
    let mut push = Command::new("git");
    push.arg("-C").arg(repo).args(["push", "-u", &args.remote, branch]).env("GIT_TERMINAL_PROMPT", "0");
    let out = crate::proc::run_bounded(push, NETWORK_TIMEOUT)?;
    if out.code != Some(0) {
        bail!("git push {} {branch} failed: {}", args.remote, out.stderr.trim());
    }
    if !args.pr {
        return Ok((true, None));
    }
    let body_file = tempfile::NamedTempFile::new()?;
    std::fs::write(body_file.path(), body)?;
    let gh = crate::omp::which("gh").ok_or_else(|| anyhow!("gh not on PATH"))?;
    let mut cmd = Command::new(gh);
    cmd.current_dir(repo).args(["pr", "create", "--fill", "--head", branch, "--body-file"]).arg(body_file.path());
    let out = crate::proc::run_bounded(cmd, NETWORK_TIMEOUT)?;
    if out.code != Some(0) {
        bail!("branch pushed, but gh pr create failed: {}", out.stderr.trim());
    }
    Ok((true, out.stdout.lines().rev().find(|l| l.starts_with("http")).map(str::to_string)))
}

/// `existing` (a repo's AGENTS.md) with `fragment` as the section `## rematch: <name>`.
/// An existing section of that heading — up to the next `#`/`##` heading outside a code
/// fence — is replaced in place; otherwise the section is appended.
pub fn upsert_agents_section(existing: &str, name: &str, fragment: &str) -> String {
    let heading = format!("## rematch: {name}");
    let section = format!("{heading}\n\n{}\n", fragment.trim());
    let top_level = |l: &str| {
        let h = l.chars().take_while(|&c| c == '#').count();
        (1..=2).contains(&h) && l[h..].chars().next().is_none_or(char::is_whitespace)
    };
    let lines: Vec<&str> = existing.lines().collect();
    let (mut start, mut end, mut fence) = (None, lines.len(), false);
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        match start {
            None if l.trim_end() == heading => start = Some(i),
            Some(_) if top_level(l) => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let join = |ls: &[&str]| ls.iter().map(|l| format!("{l}\n")).collect::<String>();
    match start {
        Some(s) => {
            let mut out = join(&lines[..s]);
            out.push_str(&section);
            if end < lines.len() {
                out.push('\n');
                out.push_str(&join(&lines[end..]));
            }
            out
        }
        None => {
            let mut out = existing.trim_end().to_string();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&section);
            out
        }
    }
}

fn ship_team(ctx: &AppCtx, e: &Entry, repo: &Path, args: &ShipArgs) -> Result<Shipped> {
    lint_before_ship(ctx, e)?;
    let branch = branch_name(e);
    let wt = Worktree::create(repo, &branch)?;
    // A skill is a dir under `.omp/skills/`; an agent is one file under `.omp/agents/`.
    let agent = e.kind == Kind::Agent;
    let rel_root = if agent { format!(".omp/agents/{}.md", e.name) } else { format!(".omp/skills/{}", e.name) };
    let dst = wt.path.join(&rel_root);
    if dst.is_dir() {
        std::fs::remove_dir_all(&dst)?;
    }
    let mut rels = Vec::new();
    let mut fragment = None;
    for (rel, abs) in package_files(&e.path)? {
        if rel == AGENTS_FRAGMENT {
            fragment = Some(std::fs::read_to_string(&abs)?);
            continue;
        }
        if agent && rel != AGENT_MD {
            continue;
        }
        let (p, shipped_rel) = if agent { (dst.clone(), rel_root.clone()) } else { (dst.join(&rel), format!("{rel_root}/{rel}")) };
        std::fs::create_dir_all(p.parent().expect("file under the worktree"))?;
        if rel == SKILL_MD || rel == AGENT_MD {
            let mut doc = SkillDoc::parse(&std::fs::read_to_string(&abs)?)?;
            doc.set_rematch(&RematchBlock {
                id: e.id.clone(),
                status: e.status,
                branch: e.branch.clone(),
                family: e.family.clone(),
                version: e.version,
                predicate_hash: predicate_hash(&e.predicate),
            });
            if let Some(serde_yaml::Value::Mapping(m)) = doc.frontmatter.get_mut("rematch") {
                m.insert("scope".into(), "team".into());
            }
            std::fs::write(&p, doc.render())?;
        } else {
            std::fs::copy(&abs, &p)?;
        }
        non_executable(&p)?;
        rels.push(shipped_rel);
    }
    let mut paths = vec![rel_root.clone()];
    if let Some(text) = fragment {
        let file = wt.path.join("AGENTS.md");
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        std::fs::write(&file, upsert_agents_section(&existing, &e.name, &text))?;
        non_executable(&file)?;
        rels.push("AGENTS.md".into());
        paths.push("AGENTS.md".into());
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    git(&wt.path, &[&["add", "--"][..], &refs].concat())?;
    let body = evidence_body(ctx, e)?;
    let message = format!("rematch: share {} with the team\n\n{body}", e.name);
    let commit = git_commit(&wt.path, &message, &refs)?;
    let hashes = file_hashes(&wt.path, &rels)?;
    let top = wt.repo.clone();
    drop(wt);
    let receipt = record_receipt(ctx, e, "ship-team", &top, &branch, &commit, &hashes)?;
    let (pushed, pr_url) = publish(&top, &branch, &message, args)?;
    Ok(Shipped { entry: e.id.clone(), name: e.name.clone(), repo: top, branch, commit, files: rels, pushed, pr_url, receipt })
}

/// Counters "at creation": the entry's predicate evaluated over the ledger sessions
/// that ended before it existed (same recurrence/harm rules as `counters::fold_session`),
/// per family — the baseline the after-numbers are compared to.
pub fn before_numbers(ctx: &AppCtx, e: &Entry) -> Result<Numbers> {
    let mut n = Numbers::default();
    for s in ctx.ledger.list_sessions(&SessionFilter::default())? {
        if s.started_at >= e.created_at {
            continue;
        }
        if !count_match(&e.predicate, &Place::of_session(&s), &s.exts, &s.stems) {
            continue;
        }
        let fam = s.family.clone().unwrap_or_else(|| Family(rematch_core::counters::UNKNOWN_FAMILY.into()));
        let r = recurred(e, &s) as u32;
        let h = harmed(&s, false) as u32;
        n.matched += 1;
        n.recurrence += r;
        n.harm += h;
        let f = n.per_family.entry(fam).or_default();
        f.matched += 1;
        f.recurrence += r;
        f.harm += h;
    }
    Ok(n)
}

/// The text a fork entry ships: `fork/<file name of fork_path>` when the package has
/// it, else the SKILL.md (agents: AGENT.md) body without frontmatter.
fn fork_content(e: &Entry, fork_path: &str) -> Result<String> {
    let base = Path::new(fork_path).file_name().ok_or_else(|| anyhow!("fork_path has no file name"))?;
    let dedicated = e.path.join("fork").join(base);
    if dedicated.is_file() {
        return Ok(std::fs::read_to_string(dedicated)?);
    }
    let md = std::fs::read_to_string(e.path.join(rematch_core::library::primary_doc(e.kind)))?;
    Ok(SkillDoc::parse(&md).map(|d| d.body.trim_start().to_string()).unwrap_or(md))
}

fn omp_version() -> String {
    let mut cmd = Command::new(crate::omp::omp_bin());
    cmd.arg("--version");
    match crate::proc::run_bounded(cmd, Duration::from_secs(20)) {
        Ok(out) if out.code == Some(0) => out.stdout.trim().to_string(),
        Ok(out) => format!("unavailable (omp --version exited {:?}: {})", out.code, out.stderr.trim()),
        Err(e) => format!("unavailable ({e:#})"),
    }
}

fn ship_fork(ctx: &AppCtx, e: &Entry, repo: &Path, args: &ShipArgs) -> Result<Shipped> {
    let fork_path = e.fork_path.clone().ok_or_else(|| anyhow!("entry has no fork_path"))?;
    let rel = validate_fork_path(&fork_path)?;
    lint_before_ship(ctx, e)?;
    let content = fork_content(e, &fork_path)?;
    let base_head = git(repo, &["rev-parse", "HEAD"])?;
    let branch = branch_name(e);
    let wt = Worktree::create(repo, &branch)?;
    let dst = wt.path.join(&rel);
    std::fs::create_dir_all(dst.parent().expect("relative file path has a parent"))?;
    std::fs::write(&dst, &content)?;
    git(&wt.path, &["add", "--", &fork_path])?;
    let file_sha = sha256_hex(content.as_bytes());
    let before = before_numbers(ctx, e)?;
    let after = Numbers::from_counters(&ctx.ledger.counters(&e.id)?);
    let body = format!(
        "{}\nBuilt fingerprint:\n  omp --version: {}\n  {}: sha256 {}\n  base commit: {}\n\nBefore (predicate over sessions before the entry existed):\n  {}\nAfter (entry counters now):\n  {}\n",
        evidence_body(ctx, e)?,
        omp_version(),
        fork_path,
        file_sha,
        base_head,
        before.render(),
        after.render(),
    );
    let message = format!("rematch: {} → {}\n\n{body}", e.name, fork_path);
    let commit = git_commit(&wt.path, &message, &[&fork_path])?;
    let top = wt.repo.clone();
    drop(wt);
    let receipt = record_receipt(ctx, e, "ship-fork", &top, &branch, &commit, &[(fork_path.clone(), file_sha)])?;
    let mut updated = e.clone();
    updated.fork_repo = Some(top.clone());
    updated.fork_commit = Some(base_head);
    updated.updated_at = ctx.now;
    save_entry(ctx, &updated)?;
    let (pushed, pr_url) = publish(&top, &branch, &body, args)?;
    Ok(Shipped { entry: e.id.clone(), name: e.name.clone(), repo: top, branch, commit, files: vec![fork_path], pushed, pr_url, receipt })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{env, keep, repo, session, sh, stage};

    fn args(scope: ShipScope, repo: Option<PathBuf>, entries: Vec<String>) -> ShipArgs {
        ShipArgs { scope, repo, entries, push: false, pr: false, remote: "origin".into() }
    }

    #[test]
    fn ship_team_creates_branch_files_and_evidence_commit() {
        let t = env();
        let ctx = &t.ctx;
        let r1 = repo(&t.dir.join("alpha"));
        let r2 = repo(&t.dir.join("beta"));
        for (i, r) in [&r1, &r2].iter().enumerate() {
            ctx.ledger.upsert_session(&session(&format!("s{}", i + 1), "alpha", Some(&r.to_string_lossy()), "anthropic/claude-opus", 1_000 + i as i64)).unwrap();
        }
        let mut e = crate::scope::testutil::entry("e-team-0001", "ledger-migrations", "alpha");
        e.predicate.repo_glob = rematch_core::predicate::derive(Some(&r1.to_string_lossy()), &[], &[], None).repo_glob;
        let e = stage(ctx, &e, &[("scripts/check.sh", "cargo test -p ledger\n")]);
        keep(ctx, &e.id);
        ctx.ledger
            .add_counter(&rematch_core::ledger::CounterRow {
                entry_id: e.id.clone(),
                session_id: "s2".into(),
                family: Family("anthropic/claude-opus".into()),
                matched: 1,
                recurrence: 0,
                harm: 0,
                tokens: 10,
                turns: 2,
                ts: 2_000,
            })
            .unwrap();
        // auto-selection: predicate content recurred in alpha and beta
        assert_eq!(recurrence_repos(ctx, &e).unwrap().len(), 2);
        let out = cmd_ship(ctx, args(ShipScope::Team, None, vec![])).unwrap();
        let shipped = out["shipped"].as_array().unwrap();
        assert_eq!(shipped.len(), 1, "{out}");
        let branch = shipped[0]["branch"].as_str().unwrap();
        assert_eq!(branch, "rematch/ledger-migrations-e0001".replace("e0001", &short_id("e-team-0001")));
        assert_eq!(rematch_core::paths::canonicalize(shipped[0]["repo"].as_str().unwrap()).unwrap(), rematch_core::paths::canonicalize(&r1).unwrap());
        // user checkout untouched; branch carries the files and the evidence body
        assert!(!r1.join(".omp").exists());
        let files = sh(&r1, &["git", "ls-tree", "-r", "--name-only", branch]);
        assert!(files.contains(".omp/skills/ledger-migrations/SKILL.md"), "{files}");
        assert!(files.contains(".omp/skills/ledger-migrations/scripts/check.sh"));
        assert!(!files.contains("rematch.json"));
        let mode = sh(&r1, &["git", "ls-tree", "-r", branch, ".omp/skills/ledger-migrations/scripts/check.sh"]);
        assert!(mode.starts_with("100644"), "{mode}");
        let skill = sh(&r1, &["git", "show", &format!("{branch}:.omp/skills/ledger-migrations/SKILL.md")]);
        assert!(skill.contains("scope: team"), "{skill}");
        let body = sh(&r1, &["git", "log", "-1", "--format=%B", branch]);
        assert!(body.contains("Counters: matched 1 · recurrence 0 · harm 0"), "{body}");
        assert!(body.contains("Matched sessions: 1"));
        assert!(body.contains("Redirect class: correction"));
        assert!(body.contains("Repos where the predicate recurred: 2"));
        let receipts = ctx.ledger.list_receipts(Some(&e.id)).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].receipt.activation, Activation::Pr);
        // shipping again refuses (branch exists) and, listed explicitly, errors
        let err = cmd_ship(ctx, args(ShipScope::Team, None, vec![e.id.clone()])).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        // worktrees are cleaned up
        let wts = sh(&r1, &["git", "worktree", "list"]);
        assert_eq!(wts.lines().count(), 1, "{wts}");
    }

    #[test]
    fn ship_team_without_cross_repo_recurrence_needs_explicit_entry() {
        let t = env();
        let ctx = &t.ctx;
        let r1 = repo(&t.dir.join("solo"));
        ctx.ledger.upsert_session(&session("s1", "solo", Some(&r1.to_string_lossy()), "anthropic/claude-opus", 1_000)).unwrap();
        let e = stage(ctx, &crate::scope::testutil::entry("e-solo-0001", "solo-skill", "solo"), &[]);
        let out = cmd_ship(ctx, args(ShipScope::Team, None, vec![])).unwrap();
        assert!(out["shipped"].as_array().unwrap().is_empty());
        let out = cmd_ship(ctx, args(ShipScope::Team, None, vec![e.name.clone()])).unwrap();
        assert_eq!(out["shipped"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn agents_md_section_is_appended_then_replaced_in_place() {
        let base = "# Repo\n\nIntro.\n\n## Build\n\nmake\n";
        let once = upsert_agents_section(base, "x", "Rule one.\n");
        assert_eq!(once, "# Repo\n\nIntro.\n\n## Build\n\nmake\n\n## rematch: x\n\nRule one.\n");
        assert_eq!(upsert_agents_section(&once, "x", "Rule one.\n"), once, "idempotent");
        let mid = "# Repo\n\n## rematch: x\n\nold\n\n### nested\n\nold too\n\n## Build\n\nmake\n";
        assert_eq!(upsert_agents_section(mid, "x", "new"), "# Repo\n\n## rematch: x\n\nnew\n\n## Build\n\nmake\n");
        let fenced = "## rematch: x\n\n```md\n## not a heading\n```\n";
        assert_eq!(upsert_agents_section(fenced, "x", "new"), "## rematch: x\n\nnew\n");
        assert_eq!(upsert_agents_section("", "y", " r \n"), "## rematch: y\n\nr\n");
        let two = upsert_agents_section(&once, "y", "Rule two.");
        assert!(two.starts_with(&once) && two.ends_with("\n\n## rematch: y\n\nRule two.\n"), "{two}");
    }

    #[test]
    fn ship_team_writes_an_agent_as_one_file_under_omp_agents() {
        let t = env();
        let ctx = &t.ctx;
        let r1 = repo(&t.dir.join("agents-repo"));
        let mut e = crate::scope::testutil::entry("e-agent-0001", "log-triage", "agents-repo");
        e.kind = Kind::Agent;
        let md = "---\nname: log-triage\ndescription: Use for a long build log; returns the first failing step.\n---\nReply with the first failing step.\n";
        let e = stage(ctx, &e, &[("AGENT.md", md)]);
        let out = cmd_ship(ctx, args(ShipScope::Team, Some(r1.clone()), vec![e.id.clone()])).unwrap();
        let branch = out["shipped"][0]["branch"].as_str().unwrap();
        let files = sh(&r1, &["git", "ls-tree", "-r", "--name-only", branch]);
        assert!(files.lines().any(|f| f == ".omp/agents/log-triage.md") && !files.contains(".omp/skills"), "one file, no skill dir: {files}");
        let shipped = sh(&r1, &["git", "show", &format!("{branch}:.omp/agents/log-triage.md")]);
        let doc = SkillDoc::parse(&shipped).unwrap();
        assert_eq!(doc.get_str("name"), Some("log-triage"));
        assert!(shipped.contains("scope: team") && doc.body.contains("first failing step"), "{shipped}");
    }

    #[test]
    fn ship_fork_writes_fork_path_with_fingerprint_and_numbers() {
        let t = env();
        let ctx = &t.ctx;
        let fork = repo(&t.dir.join("fork"));
        // a session before the entry existed, with a correction redirect, matching the predicate
        let mut s = session("s1", "fork", Some(&fork.to_string_lossy()), "github-copilot/gemini-argon", 1_000);
        s.redirects.push(rematch_core::Redirect {
            session_id: "s1".into(),
            seq: 2,
            prelabel: "revert".into(),
            class: None,
            confidence: None,
            text_head: "revert that".into(),
            deliverable_head: String::new(),
            t: 1_500,
        });
        ctx.ledger.upsert_session(&s).unwrap();
        let mut e = crate::scope::testutil::entry("e-fork-0001", "compaction-default", "fork");
        e.fork_path = Some("src/prompts/compaction.md".into());
        let e = stage(ctx, &e, &[("fork/compaction.md", "Keep the last two tool results verbatim.\n")]);
        let err = cmd_ship(ctx, args(ShipScope::Fork, None, vec![e.id.clone()])).unwrap_err();
        assert!(format!("{err:#}").contains("--repo"));
        let out = cmd_ship(ctx, args(ShipScope::Fork, Some(fork.clone()), vec![])).unwrap();
        let shipped = &out["shipped"][0];
        let branch = shipped["branch"].as_str().unwrap();
        let file = sh(&fork, &["git", "show", &format!("{branch}:src/prompts/compaction.md")]);
        assert_eq!(file, "Keep the last two tool results verbatim.");
        let body = sh(&fork, &["git", "log", "-1", "--format=%B", branch]);
        assert!(body.contains("omp --version:"), "{body}");
        assert!(body.contains(&format!("sha256 {}", sha256_hex(b"Keep the last two tool results verbatim.\n"))));
        assert!(body.contains("Before (predicate over sessions before the entry existed):\n  matched 1 · recurrence 1 · harm 0"), "{body}");
        assert!(body.contains("github-copilot/gemini-argon: matched 1, recurrence 1, harm 0"));
        assert!(body.contains("After (entry counters now):\n  matched 0"));
        let saved = ctx.ledger.get_entry(&e.id).unwrap().unwrap();
        assert_eq!(saved.fork_commit.as_deref(), Some(sh(&fork, &["git", "rev-parse", "HEAD"]).as_str()));
        assert!(rematch_core::library::load_entry(&saved.path).unwrap().fork_repo.is_some());
    }

    #[test]
    fn ship_pr_requires_push() {
        let t = env();
        let mut a = args(ShipScope::Team, None, vec![]);
        a.pr = true;
        assert!(format!("{:#}", cmd_ship(&t.ctx, a).unwrap_err()).contains("--push"));
    }
}
