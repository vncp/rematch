//! Shared application context: resolved roots, the open ledger, the clock and the
//! working directory every subcommand runs against.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use evolve_core::ledger::{now_ms, Ledger};
use evolve_core::paths::EvolvePaths;

pub struct AppCtx {
    pub paths: EvolvePaths,
    pub ledger: Ledger,
    pub now: i64,
    pub cwd: PathBuf,
    pub text: bool,
}

/// Improver files shipped with the binary (DESIGN §1: copied to `EVOLVE_ROOT/improver/`
/// on first run). `$OMP_EVOLVE_IMPROVER` points at an alternative source directory.
pub const IMPROVER_FILES: &[(&str, &str)] = &[
    ("editor.md", include_str!("../../../improver/editor.md")),
    ("reflector.md", include_str!("../../../improver/reflector.md")),
    ("classifier.md", include_str!("../../../improver/classifier.md")),
    ("lint.toml", include_str!("../../../improver/lint.toml")),
    ("omp-no-tools.yml", include_str!("../../../improver/omp-no-tools.yml")),
];

impl AppCtx {
    /// Context for the process cwd, JSON output.
    pub fn open() -> Result<AppCtx> {
        Self::open_with(None, false)
    }

    pub fn open_with(cwd: Option<PathBuf>, text: bool) -> Result<AppCtx> {
        let paths = EvolvePaths::resolve();
        paths.ensure_dirs()?;
        install_improver(&paths.improver)?;
        let ledger = Ledger::open(&paths.ledger_file)?;
        let cwd = match cwd {
            Some(c) => absolute(&c)?,
            None => std::env::current_dir().context("current dir")?,
        };
        Ok(AppCtx { paths, ledger, now: now_ms(), cwd, text })
    }

    pub fn improver_file(&self, name: &str) -> PathBuf {
        self.paths.improver.join(name)
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

/// Copy each improver file that does not exist yet (user edits are never overwritten).
fn install_improver(dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    let source = std::env::var_os("OMP_EVOLVE_IMPROVER").map(PathBuf::from).filter(|p| p.is_dir());
    for (name, builtin) in IMPROVER_FILES {
        let target = dst.join(name);
        if target.exists() {
            continue;
        }
        match source.as_ref().map(|s| s.join(name)).filter(|p| p.is_file()) {
            Some(src) => {
                std::fs::copy(&src, &target).with_context(|| format!("copy {}", src.display()))?;
            }
            None => std::fs::write(&target, builtin).with_context(|| format!("write {}", target.display()))?,
        }
    }
    Ok(())
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Required fields of a new entry; every optional field takes its serde default.
pub struct NewEntry {
    pub kind: evolve_core::types::Kind,
    pub name: String,
    pub branch: String,
    pub status: evolve_core::types::Status,
    pub family: evolve_core::types::Family,
    pub origin: evolve_core::types::Origin,
    pub predicate: evolve_core::types::Predicate,
    pub trigger_line: String,
    pub path: PathBuf,
}

/// Build an [`evolve_core::types::Entry`] (id uuid v7, personal scope, version 1).
/// Built through serde so optional fields added to `Entry` default without touching
/// every constructor.
pub fn new_entry(n: NewEntry, now: i64) -> Result<evolve_core::types::Entry> {
    let v = serde_json::json!({
        "id": new_id(),
        "kind": n.kind,
        "name": n.name,
        "branch": n.branch,
        "scope": evolve_core::types::Scope::Personal,
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
