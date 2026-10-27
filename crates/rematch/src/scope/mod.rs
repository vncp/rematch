//! Phase 3 scope (DESIGN §14 `scope/`, SPEC §Scope and who merges, §Branches,
//! §Cross-domain transfer, §Models): team/fork PRs (`ship`), the work↔home export
//! (`export`), evidence drift (`stale`), per-family splits and the capability floor
//! (`split`), cross-branch transfer and `shared` promotion (`transfer`), and the anchor
//! replay that gates a replacement of a kept entry (`anchor`, SPEC §Rollback and anchors).

pub mod anchor;
pub mod export;
pub mod ship;
pub mod split;
pub mod stale;
pub mod transfer;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use rematch_core::library::{primary_doc, AGENT_MD, REMATCH_JSON, SKILL_MD};
use rematch_core::predicate::literal_path;
use rematch_core::lint::{lint_entry, LintConfig, LintContext};
use rematch_core::package::SkillDoc;
use rematch_core::{Edit, Entry, LintReport, Origin};

use crate::app::AppCtx;

/// Files that stay with an entry's history and never travel with a copy: the ledger
/// mirror (absolute paths, evidence ids) and the reflector's EXPERIENCE.md.
const NOT_COPIED: &[&str] = &[REMATCH_JSON, "EXPERIENCE.md"];

/// Run a local git command (never network) in `dir`; stdout trimmed on success.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} in {} failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `-c user.name/-c user.email` fallbacks when the repo has no committer identity, so
/// commits never fail on a fresh machine; an configured identity always wins.
pub(crate) fn git_identity_args(dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    if git(dir, &["config", "--get", "user.name"]).map_or(true, |s| s.is_empty()) {
        v.extend(["-c".to_string(), "user.name=rematch".to_string()]);
    }
    if git(dir, &["config", "--get", "user.email"]).map_or(true, |s| s.is_empty()) {
        v.extend(["-c".to_string(), "user.email=rematch@localhost".to_string()]);
    }
    v
}

/// Commit `paths` (only those, whatever else is staged) in `dir` with `message`
/// (subject + body); returns the new commit sha.
pub(crate) fn git_commit(dir: &Path, message: &str, paths: &[&str]) -> Result<String> {
    let msg = tempfile::NamedTempFile::new()?;
    std::fs::write(msg.path(), message)?;
    let mut args = git_identity_args(dir);
    args.extend(["commit".into(), "--no-verify".into(), "-q".into(), "-F".into()]);
    args.push(msg.path().to_string_lossy().to_string());
    args.push("--".into());
    args.extend(paths.iter().map(|p| p.to_string()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(dir, &refs)?;
    git(dir, &["rev-parse", "HEAD"])
}

/// Last 8 alphanumerics of an id (uuid v7 heads are timestamps and collide).
pub(crate) fn short_id(id: &str) -> String {
    let alnum: Vec<char> = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    alnum[alnum.len().saturating_sub(8)..].iter().collect()
}

fn rel_of(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Every file of an entry directory as `(relative path, absolute path)`, sorted,
/// without `rematch.json` / `EXPERIENCE.md`.
pub(crate) fn package_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        bail!("entry dir missing: {}", dir.display());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let p = ent?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let r = rel_of(dir, &p);
                if !NOT_COPIED.contains(&r.as_str()) {
                    out.push((r, p));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// An entry's package as `Edit::Write`s (input to `library::stage` for copies).
pub(crate) fn package_edits(dir: &Path) -> Result<Vec<Edit>> {
    package_files(dir)?
        .into_iter()
        .map(|(path, abs)| {
            let content = std::fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;
            Ok(Edit::Write { path, content })
        })
        .collect()
}

pub(crate) fn edit_content<'a>(edits: &'a [Edit], rel: &str) -> Option<&'a str> {
    edits.iter().find_map(|e| match e {
        Edit::Write { path, content } if path == rel => Some(content.as_str()),
        _ => None,
    })
}

pub(crate) fn put_edit(edits: &mut Vec<Edit>, rel: &str, content: String) {
    edits.retain(|e| !matches!(e, Edit::Write { path, .. } | Edit::Delete { path } if path == rel));
    edits.push(Edit::Write { path: rel.to_string(), content });
}

/// Copies under a new name must not reuse the original's SKILL.md / AGENT.md `name:`
/// (omp would see two skills or agents with one name) nor its `rematch:` block (stage
/// rewrites it).
pub(crate) fn rename_skill(edits: &mut Vec<Edit>, name: &str) -> Result<()> {
    for doc_name in [SKILL_MD, AGENT_MD] {
        if let Some(md) = edit_content(edits, doc_name) {
            let mut doc = SkillDoc::parse(md)?;
            doc.set("name", name);
            doc.frontmatter.remove("rematch");
            let text = doc.render();
            put_edit(edits, doc_name, text);
        }
    }
    Ok(())
}

/// Resolve a user reference: exact id, else the newest entry with that name, preferring
/// live (kept/provisional/pinned) ones over archived versions of the same name.
pub(crate) fn resolve_entry(ctx: &AppCtx, id_or_name: &str) -> Result<Entry> {
    if let Some(e) = ctx.ledger.get_entry(id_or_name)? {
        return Ok(e);
    }
    let mut named = ctx.ledger.list_entries(&rematch_core::ledger::EntryFilter {
        name: Some(id_or_name.to_string()),
        ..Default::default()
    })?;
    let live = |e: &Entry| matches!(e.status, rematch_core::Status::Kept | rematch_core::Status::Provisional | rematch_core::Status::Pinned);
    match named.iter().rposition(live) {
        Some(i) => Ok(named.swap_remove(i)),
        None => named.pop().ok_or_else(|| anyhow::anyhow!("no entry {id_or_name:?}")),
    }
}

/// The entry's directory holds files rematch wrote: not foreign (`reflect::is_foreign`:
/// outside the rematch root and the kept-skills dir, or a lesson) and carrying our own
/// `rematch.json` for this id. A user's skill registered in place (brownfield, forge
/// import) has no rematch.json and must never be moved or written to.
pub(crate) fn owns_files(ctx: &AppCtx, e: &Entry) -> bool {
    !crate::reflect::is_foreign(ctx, e)
        && rematch_core::library::load_entry(&e.path).is_ok_and(|d| d.id == e.id)
}

/// Status change that moves directories only for entries whose files are ours; for
/// user-owned entries the status is recorded in the ledger only.
pub(crate) fn change_status(ctx: &AppCtx, e: &Entry, to: rematch_core::Status, note: &str) -> Result<Entry> {
    if owns_files(ctx, e) {
        rematch_core::library::set_status(&ctx.ledger, &ctx.paths, &e.id, to, Some(note))
    } else {
        crate::reflect::record_status(ctx, e, to, note)
    }
}

/// Mirror the entry into the ledger, and into `rematch.json` when the files are ours
/// (fields `library` has no setter for: fork drift data, family exclusions).
pub(crate) fn save_entry(ctx: &AppCtx, e: &Entry) -> Result<()> {
    if owns_files(ctx, e) {
        std::fs::write(e.path.join(REMATCH_JSON), serde_json::to_string_pretty(e)?)?;
    }
    ctx.ledger.upsert_entry(e)
}

pub(crate) fn lint_config(ctx: &AppCtx) -> Result<LintConfig> {
    LintConfig::load(&ctx.paths.improver.join("lint.toml"))
}

/// Lint a not-yet-staged copy: materialize the edits (plus the SKILL.md / AGENT.md
/// frontmatter `library::stage` would add) in a temporary dir and run `lint_entry`.
pub(crate) fn lint_edits(ctx: &AppCtx, entry: &Entry, edits: &[Edit], lctx: &LintContext) -> Result<LintReport> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    for e in edits {
        if let Edit::Write { path, content } = e {
            let p = dir.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, content)?;
        }
    }
    let primary = dir.join(primary_doc(entry.kind));
    let doc = match std::fs::read_to_string(&primary) {
        Ok(t) => Some(SkillDoc::parse(&t)?),
        // stage refuses an agent without AGENT.md; lint reports it
        Err(_) if entry.kind == rematch_core::Kind::Agent => None,
        Err(_) => Some(SkillDoc { body: format!("\n# {}\n\n{}\n", entry.name, entry.trigger_line), ..Default::default() }),
    };
    if let Some(mut doc) = doc {
        if doc.get_str("name").is_none() {
            doc.set("name", entry.name.as_str());
        }
        if doc.get_str("description").is_none_or(|d| d.trim().is_empty()) {
            doc.set("description", entry.trigger_line.as_str());
        }
        std::fs::write(&primary, doc.render())?;
    }
    if let Some(h) = &entry.hook {
        std::fs::write(dir.join("hook.json"), serde_json::to_string_pretty(h)?)?;
    }
    lint_entry(dir, entry, &lint_config(ctx)?, lctx)
}

pub(crate) fn lint_errors(rep: &LintReport) -> String {
    rep.findings
        .iter()
        .filter(|f| f.severity == rematch_core::Severity::Error)
        .map(|f| format!("[{}] {}{}", f.rule, f.message, f.path.as_deref().map(|p| format!(" ({p})")).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("; ")
}


/// The repo an entry came from: its originating session's repo, else an evidence
/// session's repo, else the exact path its repo glob names (when it exists).
pub(crate) fn origin_repo(ctx: &AppCtx, e: &Entry) -> Result<Option<PathBuf>> {
    let mut sessions: Vec<String> = Vec::new();
    if let Origin::Session { session_id } = &e.origin {
        sessions.push(session_id.clone());
    }
    sessions.extend(e.evidence.iter().map(|m| m.session_id.clone()));
    for sid in sessions {
        if let Some(repo) = ctx.ledger.get_session(&sid)?.and_then(|s| s.repo) {
            return Ok(Some(PathBuf::from(repo)));
        }
    }
    Ok(literal_path(&e.predicate.repo_glob).filter(|p| p.is_dir()))
}

/// Repo (else cwd) of the most recent ledger session in a branch.
pub(crate) fn branch_anchor(ctx: &AppCtx, branch: &str) -> Result<Option<String>> {
    let sessions = ctx.ledger.list_sessions(&rematch_core::ledger::SessionFilter {
        branch: Some(branch.to_string()),
        ..Default::default()
    })?;
    Ok(sessions.last().map(|s| s.repo.clone().unwrap_or_else(|| s.cwd.clone())))
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    use rematch_core::ledger::Ledger;
    use rematch_core::paths::RematchPaths;
    use rematch_core::{
        Edit, Entry, Family, Kind, LessonTag, Manifest, Moment, Origin, Predicate, Scope, SessionRecord, Status,
        EditorScope,
    };

    use crate::app::AppCtx;

    pub struct Env {
        pub _tmp: tempfile::TempDir,
        pub ctx: AppCtx,
        pub dir: PathBuf,
    }

    pub fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let paths = RematchPaths::with_roots(dir.join("rematch"), dir.join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        let ctx = AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: dir.clone(), text: false };
        Env { _tmp: tmp, ctx, dir }
    }

    pub fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(args[0]).args(&args[1..]).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A git repo with one commit.
    pub fn repo(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        sh(path, &["git", "init", "-q", "-b", "main"]);
        sh(path, &["git", "config", "user.name", "Test"]);
        sh(path, &["git", "config", "user.email", "test@example.com"]);
        std::fs::write(path.join("README.md"), "# repo\n").unwrap();
        sh(path, &["git", "add", "."]);
        sh(path, &["git", "commit", "-q", "-m", "init"]);
        path.to_path_buf()
    }

    pub fn entry(id: &str, name: &str, branch: &str) -> Entry {
        Entry {
            id: id.into(),
            kind: Kind::Skill,
            name: name.into(),
            branch: branch.into(),
            scope: Scope::Personal,
            status: Status::Provisional,
            family: Family("anthropic/claude-opus".into()),
            verified_on: vec![],
            helpful: 0,
            harmful: 0,
            evidence: vec![Moment { session_id: "s1".into(), seq: 3 }],
            origin: Origin::Session { session_id: "s1".into() },
            predicate: Predicate {
                repo_glob: "**".into(),
                file_exts: vec!["rs".into()],
                stems: vec!["ledger".into(), "migrat".into()],
                ..Default::default()
            },
            trigger_line: "Use when editing the ledger migrations.".into(),
            redirect_class: Some(rematch_core::RedirectClass::Correction),
            predecessor: None,
            version: 1,
            path: PathBuf::new(),
            created_at: 0,
            updated_at: 0,
            manifest: Some(Manifest {
                evidence: Moment { session_id: "s1".into(), seq: 3 },
                predicted_fix: "migration test runs before commit".into(),
                predicted_regression: "slower edits".into(),
                rationale: String::new(),
                scope: EditorScope::Invariant,
            }),
            lesson_tag: Some(LessonTag::Invariant),
            pinned: false,
            hook: None,
            approved_scripts: vec![],
            approved_fragment: None,
            approved_agent: None,
            sibling_of: None,
            status_since: None,
            status_note: None,
            pending_next_run: false,
            fork_path: None,
            fork_repo: None,
            fork_commit: None,
            families_excluded: vec![],
            links: vec![],
            content_sha256: None,
            last_tune_at: None,
            tune: None,
            retention: None,
        }
    }

    pub fn skill_md(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: Use when editing the ledger migrations.\n---\n\n# {name}\n\nUse when editing the ledger migrations.\n\n## Laws\n\n- Run the migration test before every commit.\n"
        )
    }

    pub fn stage(ctx: &AppCtx, e: &Entry, extra: &[(&str, &str)]) -> Entry {
        let mut files = vec![Edit::Write { path: "SKILL.md".into(), content: skill_md(&e.name) }];
        for (p, c) in extra {
            files.push(Edit::Write { path: p.to_string(), content: c.to_string() });
        }
        rematch_core::library::stage(&ctx.ledger, &ctx.paths, e, &files).unwrap()
    }

    pub fn keep(ctx: &AppCtx, id: &str) -> Entry {
        rematch_core::library::set_status(&ctx.ledger, &ctx.paths, id, Status::Kept, None).unwrap()
    }

    pub fn session(id: &str, branch: &str, repo: Option<&str>, family: &str, started_at: i64) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            cwd: repo.unwrap_or("/tmp/x").into(),
            repo: repo.map(str::to_string),
            branch: branch.into(),
            started_at,
            ended_at: Some(started_at + 1000),
            family: Some(Family(family.into())),
            exts: vec!["rs".into()],
            stems: vec!["ledger".into(), "migrat".into()],
            counted: true,
            source: "trace".into(),
            ..Default::default()
        }
    }
}
