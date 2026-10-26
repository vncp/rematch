//! `doctor`: omp binary + version, model roles, extension linked, ledger schema, improver
//! files, root paths, kill switch.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use evolve_core::ledger::{enum_str, EntryFilter, SessionFilter, SCHEMA_VERSION};
use serde_json::{json, Value};

use crate::app::{AppCtx, IMPROVER_FILES};
use crate::config::{self, ROLE_CLASSIFIER, ROLE_EDITOR, ROLE_SOLVER};
use crate::omp;
use crate::proc::run_bounded;

pub const PLUGIN_NAME: &str = "@vncp/omp-evolve";

fn plugins_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    std::env::var_os("OMP_PLUGINS_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".omp").join("plugins"))
}

pub fn cmd_doctor(ctx: &AppCtx) -> Result<Value> {
    let mut problems: Vec<String> = Vec::new();
    let mut lines = Vec::new();

    let bin = omp::omp_bin();
    let mut c = Command::new(&bin);
    c.arg("--version");
    let version = match run_bounded(c, Duration::from_secs(15)) {
        Ok(b) if b.code == Some(0) => Some(b.stdout.trim().to_string()),
        Ok(b) => {
            problems.push(format!("`{} --version` exited {:?}: {}", bin.display(), b.code, b.stderr.trim()));
            None
        }
        Err(e) => {
            problems.push(format!("omp not runnable at {}: {e:#}", bin.display()));
            None
        }
    };
    lines.push(format!("omp: {} ({})", bin.display(), version.as_deref().unwrap_or("not runnable")));

    let mut roles = serde_json::Map::new();
    for role in [ROLE_EDITOR, ROLE_CLASSIFIER, ROLE_SOLVER] {
        match config::resolve_role(&ctx.paths, &ctx.cwd, role) {
            Ok(r) => {
                lines.push(format!("role {role}: {} (via {})", r.selector, r.role_used));
                roles.insert(role.into(), json!({"selector": r.selector, "via": r.role_used, "model": r.model}));
            }
            Err(e) => {
                problems.push(format!("role {role}: {e:#}"));
                lines.push(format!("role {role}: unresolved"));
                roles.insert(role.into(), json!({"error": format!("{e:#}")}));
            }
        }
    }

    let plugins = plugins_dir();
    let lock = plugins.join("omp-plugins.lock.json");
    let in_lock = std::fs::read_to_string(&lock).map(|t| t.contains(&format!("\"{PLUGIN_NAME}\""))).unwrap_or(false);
    let module = plugins.join("node_modules").join(PLUGIN_NAME);
    let linked_path = std::fs::read_link(&module).ok();
    let module_exists = module.exists();
    let linked = in_lock || module_exists;
    if !linked {
        problems.push(format!("extension not linked: run `omp plugin link <repo>/extension` ({PLUGIN_NAME} missing from {})", lock.display()));
    }
    lines.push(format!("extension {PLUGIN_NAME}: {}", if linked { "linked" } else { "not linked" }));

    let schema = ctx.ledger.schema_version()?;
    if schema != SCHEMA_VERSION {
        problems.push(format!("ledger schema {schema}, binary expects {SCHEMA_VERSION}"));
    }
    lines.push(format!("ledger: {} (schema {schema})", ctx.paths.ledger_file.display()));

    let improver: Vec<Value> = IMPROVER_FILES
        .iter()
        .map(|(name, _)| {
            let p = ctx.paths.improver.join(name);
            let present = p.is_file();
            if !present {
                problems.push(format!("improver file missing: {}", p.display()));
            }
            json!({"file": name, "present": present})
        })
        .collect();
    if let Err(e) = evolve_core::lint::LintConfig::load(&ctx.paths.improver.join("lint.toml")) {
        problems.push(format!("lint.toml invalid: {e:#}"));
    }

    let sessions = ctx.ledger.list_sessions(&SessionFilter::default())?;
    let traced = sessions.iter().filter(|s| s.source == "trace").count();
    let mut by_status = std::collections::BTreeMap::<String, u32>::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        *by_status.entry(enum_str(&e.status)).or_default() += 1;
    }
    let pending = std::fs::read_dir(&ctx.paths.pending)
        .map(|r| r.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).count())
        .unwrap_or(0);
    let off = ctx.paths.is_off();
    lines.push(format!(
        "root: {} · {} sessions ({} traced) · {} pending · off: {off}",
        ctx.paths.root.display(),
        sessions.len(),
        traced,
        pending
    ));
    for p in &problems {
        lines.push(format!("problem: {p}"));
    }

    Ok(json!({
        "healthy": problems.is_empty(),
        "problems": problems,
        "omp": {"bin": bin, "version": version},
        "roles": roles,
        "extension": {"linked": linked, "lock_file": lock, "in_lock": in_lock, "module": module, "module_exists": module_exists, "link_target": linked_path},
        "ledger": {"path": ctx.paths.ledger_file, "schema_version": schema, "binary_schema_version": SCHEMA_VERSION,
                   "sessions": sessions.len(), "traced_sessions": traced, "entries": by_status},
        "improver": {"dir": ctx.paths.improver, "files": improver},
        "paths": {"root": ctx.paths.root, "agent_dir": ctx.paths.agent_dir, "traces": ctx.paths.traces, "pending": ctx.paths.pending,
                  "staging": ctx.paths.staging, "drafts": ctx.paths.drafts, "archive": ctx.paths.archive, "jobs": ctx.paths.jobs,
                  "skills": ctx.paths.skills_dir(), "hooks": ctx.paths.hooks_dir()},
        "pending": pending,
        "off": off,
        "version": env!("CARGO_PKG_VERSION"),
        "lines": lines,
    }))
}
