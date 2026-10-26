//! Root resolution (DESIGN §1): `REMATCH_ROOT` = `$REMATCH_ROOT` or `~/.rematch`;
//! agent dir = `$PI_CODING_AGENT_DIR` or `~/.omp/agent`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RematchPaths {
    pub root: PathBuf,
    pub agent_dir: PathBuf,
    pub traces: PathBuf,
    pub pending: PathBuf,
    pub staging: PathBuf,
    pub drafts: PathBuf,
    pub archive: PathBuf,
    pub predecessors: PathBuf,
    /// Symlinks `<name>.md → <entry dir>/AGENT.md` of provisional agents; the plugin's
    /// `extension/agents` links here, so omp discovers them from the extension root.
    pub agents: PathBuf,
    /// Entry dirs of kept agents (omp loads the copy in `<agent_dir>/agents/<name>.md`).
    pub kept_agents: PathBuf,
    pub improver: PathBuf,
    pub jobs: PathBuf,
    pub runs: PathBuf,
    pub tasks: PathBuf,
    pub branches: PathBuf,
    pub export: PathBuf,
    pub ledger_file: PathBuf,
    pub blobs: PathBuf,
    pub off_marker: PathBuf,
}

impl RematchPaths {
    /// Resolve from the process environment.
    pub fn resolve() -> RematchPaths {
        Self::from_env(|k| std::env::var(k).ok())
    }

    /// Resolve from an arbitrary environment lookup (empty values count as unset).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> RematchPaths {
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        let home = get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let root = get("REMATCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".rematch"));
        let agent_dir = get("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".omp").join("agent"));
        Self::with_roots(root, agent_dir)
    }

    pub fn with_roots(root: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> RematchPaths {
        let root = root.into();
        let agent_dir = agent_dir.into();
        RematchPaths {
            traces: root.join("traces"),
            pending: root.join("pending"),
            staging: root.join("staging"),
            drafts: root.join("drafts"),
            archive: root.join("archive"),
            predecessors: root.join("predecessors"),
            agents: root.join("agents"),
            kept_agents: root.join("kept-agents"),
            improver: root.join("improver"),
            jobs: root.join("jobs"),
            runs: root.join("runs"),
            tasks: root.join("tasks"),
            branches: root.join("branches"),
            export: root.join("export"),
            ledger_file: root.join("ledger.sqlite"),
            blobs: root.join("blobs"),
            off_marker: root.join("off"),
            root,
            agent_dir,
        }
    }

    /// Create every directory under the root (not the agent dir's subtrees, which are
    /// created lazily when an entry is kept or a hook approved).
    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.root,
            &self.traces,
            &self.pending,
            &self.staging,
            &self.drafts,
            &self.archive,
            &self.predecessors,
            &self.agents,
            &self.kept_agents,
            &self.improver,
            &self.jobs,
            &self.runs,
            &self.tasks,
            &self.branches,
            &self.export,
            &self.blobs,
        ] {
            std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
        }
        Ok(())
    }

    /// Kill switch: `REMATCH=0` or the `off` marker file.
    pub fn is_off(&self) -> bool {
        Self::off_by(std::env::var("REMATCH").ok().as_deref(), &self.off_marker)
    }

    fn off_by(env: Option<&str>, marker: &Path) -> bool {
        matches!(env.map(str::trim), Some("0") | Some("false") | Some("off")) || marker.exists()
    }

    /// Kept skills live natively here.
    pub fn skills_dir(&self) -> PathBuf {
        self.agent_dir.join("skills")
    }

    /// Kept agents live natively here as `<name>.md` (omp's user agent dir).
    pub fn agents_dir(&self) -> PathBuf {
        self.agent_dir.join("agents")
    }

    /// Compiled verifiers (inert to omp; imported by the extension's `tool_call` handler).
    pub fn hooks_dir(&self) -> PathBuf {
        self.agent_dir.join("extensions").join("rematch-hooks")
    }

    pub fn staging_branch(&self, branch: &str) -> PathBuf {
        self.staging.join(branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_derive_from_home() {
        let p = RematchPaths::from_env(env(&[("HOME", "/h")]));
        assert_eq!(p.root, PathBuf::from("/h/.rematch"));
        assert_eq!(p.agent_dir, PathBuf::from("/h/.omp/agent"));
        assert_eq!(p.ledger_file, PathBuf::from("/h/.rematch/ledger.sqlite"));
        assert_eq!(p.hooks_dir(), PathBuf::from("/h/.omp/agent/extensions/rematch-hooks"));
    }

    #[test]
    fn overrides_win_and_empty_is_unset() {
        let p = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("REMATCH_ROOT", "/r"),
            ("PI_CODING_AGENT_DIR", ""),
        ]));
        assert_eq!(p.root, PathBuf::from("/r"));
        assert_eq!(p.off_marker, PathBuf::from("/r/off"));
        assert_eq!(p.agent_dir, PathBuf::from("/h/.omp/agent"));
    }

    #[test]
    fn off_by_env_or_marker() {
        let t = tempfile::tempdir().unwrap();
        let p = RematchPaths::with_roots(t.path().join("root"), t.path().join("agent"));
        p.ensure_dirs().unwrap();
        assert!(p.staging.is_dir() && p.blobs.is_dir() && p.agents.is_dir() && p.kept_agents.is_dir());
        assert!(!RematchPaths::off_by(None, &p.off_marker));
        assert!(!RematchPaths::off_by(Some("1"), &p.off_marker));
        assert!(RematchPaths::off_by(Some("0"), &p.off_marker));
        std::fs::write(&p.off_marker, "").unwrap();
        assert!(RematchPaths::off_by(Some("1"), &p.off_marker));
    }
}
