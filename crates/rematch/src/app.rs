//! Shared application context: resolved roots, the open ledger, the clock and the
//! working directory every subcommand runs against.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rematch_core::ledger::{now_ms, sha256_hex, Ledger};
use rematch_core::domain::DomainPolicy;
use rematch_core::paths::RematchPaths;

pub struct AppCtx {
    pub paths: RematchPaths,
    pub ledger: Ledger,
    pub now: i64,
    pub cwd: PathBuf,
    pub text: bool,
}

/// Improver files shipped with the binary (DESIGN §1: copied to `REMATCH_ROOT/improver/`
/// on first run). `$REMATCH_IMPROVER` points at an alternative source directory.
pub const IMPROVER_FILES: &[(&str, &str)] = &[
    ("editor.md", include_str!("../../../improver/editor.md")),
    ("composer.md", include_str!("../../../improver/composer.md")),
    ("reflector.md", include_str!("../../../improver/reflector.md")),
    ("classifier.md", include_str!("../../../improver/classifier.md")),
    ("lint.toml", include_str!("../../../improver/lint.toml")),
    ("domain_rules.toml", include_str!("../../../improver/domain_rules.toml")),
    ("omp-no-tools.yml", include_str!("../../../improver/omp-no-tools.yml")),
];

impl AppCtx {
    /// Context for the process cwd, JSON output.
    pub fn open() -> Result<AppCtx> {
        Self::open_with(None, false)
    }

    pub fn open_with(cwd: Option<PathBuf>, text: bool) -> Result<AppCtx> {
        let paths = RematchPaths::resolve();
        paths.ensure_dirs()?;
        install_improver(&paths.improver)?;
        let ledger = Ledger::open(&paths.ledger_file)?;
        let cwd = match cwd {
            Some(c) => absolute(&c)?,
            None => std::env::current_dir().context("current dir")?,
        };
        let ctx = AppCtx { paths, ledger, now: now_ms(), cwd, text };
        // Built-in entries arrive unapproved; a failure must not take every command down.
        if let Err(e) = crate::builtins::ensure(&ctx) {
            eprintln!("rematch: built-in entries not installed: {e:#}");
        }
        Ok(ctx)
    }

    pub fn improver_file(&self, name: &str) -> PathBuf {
        self.paths.improver.join(name)
    }

    /// `policy.json` domains and caps, branch `domain.yml` files, `improver/domain_rules.toml`.
    pub fn domain_policy(&self) -> Result<DomainPolicy> {
        DomainPolicy::load(&self.paths)
    }
}

/// Absolute, lexically cleaned path (no filesystem access beyond cwd).
pub fn absolute(p: &Path) -> Result<PathBuf> {
    let joined = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Installs the shipped improver files. A file the system wrote and the user never touched
/// (its sha256 equals the recorded shipped hash in `.shipped/<name>`) is refreshed when the
/// binary ships a newer version; a file the user edited is left alone. Files from older
/// installs without a record are treated as shipped. The system itself never amends these
/// files (D25): only a new binary release does.
fn install_improver(dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst.join(".shipped"))?;
    let source = std::env::var_os("REMATCH_IMPROVER").map(PathBuf::from).filter(|p| p.is_dir());
    for (name, builtin) in IMPROVER_FILES {
        let target = dst.join(name);
        let record = dst.join(".shipped").join(name);
        let content: String = match source.as_ref().map(|s| s.join(name)).filter(|p| p.is_file()) {
            Some(src) => std::fs::read_to_string(&src).with_context(|| format!("read {}", src.display()))?,
            None => (*builtin).to_string(),
        };
        let new_hash = sha256_hex(content.as_bytes());
        if target.exists() {
            let current = std::fs::read_to_string(&target).unwrap_or_default();
            let current_hash = sha256_hex(current.as_bytes());
            if current_hash == new_hash {
                let _ = std::fs::write(&record, &new_hash);
                continue;
            }
            let shipped = std::fs::read_to_string(&record).ok().map(|s| s.trim().to_string());
            let user_edited = shipped.is_some_and(|h| h != current_hash);
            if user_edited {
                continue;
            }
        }
        std::fs::write(&target, &content).with_context(|| format!("write {}", target.display()))?;
        std::fs::write(&record, &new_hash)?;
    }
    Ok(())
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Required fields of a new entry; every optional field takes its serde default.
pub struct NewEntry {
    pub kind: rematch_core::types::Kind,
    pub name: String,
    pub branch: String,
    pub status: rematch_core::types::Status,
    pub family: rematch_core::types::Family,
    pub origin: rematch_core::types::Origin,
    pub predicate: rematch_core::types::Predicate,
    pub trigger_line: String,
    pub path: PathBuf,
}

/// Build a [`rematch_core::types::Entry`] (id uuid v7, personal scope, version 1).
/// Built through serde so optional fields added to `Entry` default without touching
/// every constructor.
pub fn new_entry(n: NewEntry, now: i64) -> Result<rematch_core::types::Entry> {
    let v = serde_json::json!({
        "id": new_id(),
        "kind": n.kind,
        "name": n.name,
        "branch": n.branch,
        "scope": rematch_core::types::Scope::Personal,
        "status": n.status,
        "family": n.family,
        "origin": n.origin,
        "predicate": n.predicate,
        "trigger_line": n.trigger_line,
        "version": 1,
        "path": n.path,
        "created_at": now,
        "updated_at": now,
    });
    Ok(serde_json::from_value(v)?)
}
