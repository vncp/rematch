//! omp `config.yml` → `modelRoles` lookup (DESIGN §0): `<agent_dir>/config.yml` with
//! `<repo>/.omp/config.yml` overlaid; custom roles fall back along fixed chains.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use evolve_core::paths::EvolvePaths;
use evolve_core::types::ModelRef;
use serde::Serialize;

pub const ROLE_EDITOR: &str = "evolve-editor";
pub const ROLE_CLASSIFIER: &str = "evolve-classifier";
pub const ROLE_SOLVER: &str = "evolve-solver";

const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleRef {
    /// Role whose entry resolved (`evolve-editor`, `slow`, `default`, …).
    pub role_used: String,
    pub model: ModelRef,
    /// `provider/id` as passed to `--model` (thinking suffix dropped).
    pub selector: String,
}

/// Fallback chain of a role (DESIGN §0).
pub fn chain_for(role: &str) -> Vec<&str> {
    match role {
        ROLE_EDITOR => vec![ROLE_EDITOR, "slow", "default"],
        ROLE_CLASSIFIER => vec![ROLE_CLASSIFIER, "smol", "tiny", "default"],
        ROLE_SOLVER => vec![ROLE_SOLVER, "default"],
        "default" => vec!["default"],
        other => vec![other, "default"],
    }
}

/// `provider/model[:thinking]` → ModelRef (thinking level dropped).
pub fn parse_selector(s: &str) -> Result<ModelRef> {
    let s = s.trim();
    let (provider, mut id) = s.split_once('/').ok_or_else(|| anyhow!("model selector {s:?} is not provider/model"))?;
    if let Some((base, level)) = id.rsplit_once(':') {
        if THINKING_LEVELS.contains(&level.to_ascii_lowercase().as_str()) {
            id = base;
        }
    }
    if provider.trim().is_empty() || id.trim().is_empty() {
        bail!("model selector {s:?} is not provider/model");
    }
    Ok(ModelRef { provider: provider.trim().to_string(), id: id.trim().to_string() })
}

/// Config files consulted, in overlay order (later wins).
pub fn config_files(paths: &EvolvePaths, cwd: &Path) -> Vec<PathBuf> {
    let mut v = vec![paths.agent_dir.join("config.yml")];
    let base = evolve_core::import::repo_of(cwd).unwrap_or_else(|| cwd.to_path_buf());
    v.push(base.join(".omp").join("config.yml"));
    v
}

/// Merged `modelRoles` map.
pub fn model_roles(paths: &EvolvePaths, cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut roles = BTreeMap::new();
    for f in config_files(paths, cwd) {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&text).map_err(|e| anyhow!("parse {}: {e}", f.display()))?;
        if let Some(m) = doc.get("modelRoles").and_then(|v| v.as_mapping()) {
            for (k, v) in m {
                if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                    roles.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    Ok(roles)
}

pub fn resolve_chain(paths: &EvolvePaths, cwd: &Path, chain: &[&str]) -> Result<RoleRef> {
    let roles = model_roles(paths, cwd)?;
    for role in chain {
        if let Some(sel) = roles.get(*role).filter(|s| !s.trim().is_empty()) {
            let model = parse_selector(sel)?;
            let selector = format!("{}/{}", model.provider, model.id);
            return Ok(RoleRef { role_used: role.to_string(), model, selector });
        }
    }
    bail!(
        "no model configured for roles {} (set modelRoles in {})",
        chain.join(" → "),
        paths.agent_dir.join("config.yml").display()
    )
}

/// Resolve a role through its built-in fallback chain.
pub fn resolve_role(paths: &EvolvePaths, cwd: &Path, role: &str) -> Result<RoleRef> {
    resolve_chain(paths, cwd, &chain_for(role))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_drops_thinking() {
        let m = parse_selector("anthropic/claude-opus-5-5:high").unwrap();
        assert_eq!((m.provider.as_str(), m.id.as_str()), ("anthropic", "claude-opus-5-5"));
        let m = parse_selector("bedrock/model:v2").unwrap();
        assert_eq!(m.id, "model:v2");
        assert!(parse_selector("nonsense").is_err());
    }

    #[test]
    fn chains_and_overlay() {
        let t = tempfile::tempdir().unwrap();
        let paths = EvolvePaths::with_roots(t.path().join("ev"), t.path().join("agent"));
        std::fs::create_dir_all(&paths.agent_dir).unwrap();
        std::fs::write(
            paths.agent_dir.join("config.yml"),
            "modelRoles:\n  smol: p/small\n  default: anthropic/claude-opus-5-5:high\n",
        )
        .unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".omp")).unwrap();
        let ed = resolve_role(&paths, &repo, ROLE_EDITOR).unwrap();
        assert_eq!((ed.role_used.as_str(), ed.selector.as_str()), ("default", "anthropic/claude-opus-5-5"));
        let cl = resolve_role(&paths, &repo, ROLE_CLASSIFIER).unwrap();
        assert_eq!(cl.role_used, "smol");
        std::fs::write(repo.join(".omp/config.yml"), "modelRoles:\n  evolve-editor: x/big\n").unwrap();
        let ed = resolve_role(&paths, &repo.join("sub"), ROLE_EDITOR);
        // `sub` does not exist: repo_of walks up lexically from the given path.
        let ed = ed.unwrap();
        assert_eq!(ed.selector, "x/big");
    }
}
