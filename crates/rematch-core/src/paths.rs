//! Root resolution (DESIGN §1, `omp://config-usage.md#profiles`, `omp://extension-loading.md`):
//! - `REMATCH_ROOT` = `$REMATCH_ROOT` or `~/.rematch` (never per profile).
//! - `config_root` = `~/.omp`, or `~/<PI_CONFIG_DIR>` when `PI_CONFIG_DIR` is set (or `<PI_CONFIG_DIR>` if absolute).
//! - Active profile = `OMP_PROFILE` when present in the environment (empty/`"default"` selects the default profile and suppresses `PI_PROFILE`), else `PI_PROFILE`; invalid or `"default"`/empty ⇒ `None`.
//! - `agent_dir` = `<config_root>/profiles/<name>/agent` when a named profile is active (`PI_CODING_AGENT_DIR` is ignored for named profiles); else `$PI_CODING_AGENT_DIR` when set and not equal to a profile-derived `<config_root>/profiles/<p>/agent`; else `<config_root>/agent`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RematchPaths {
    pub root: PathBuf,
    pub config_root: PathBuf,
    pub profile: Option<String>,
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
    /// The one skill delivery directory every harness loads (DESIGN §16 "Shared skills"):
    /// symlinks `<name> → <entry dir>` written by `library::sync_shared_skills`.
    pub skills_shared: PathBuf,
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

    /// Resolve from an arbitrary environment lookup (`None` = unset, `Some("")` = set empty).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> RematchPaths {
        let non_empty = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let home = non_empty("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let root = non_empty("REMATCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".rematch"));
        let config_dir = non_empty("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".to_string());
        let config_root = home.join(&config_dir);
        let omp_profile = get("OMP_PROFILE");
        let pi_profile = get("PI_PROFILE");
        let profile = resolve_profile_env(omp_profile.as_deref(), pi_profile.as_deref());
        let pi_profile_norm = pi_profile.as_deref().and_then(normalize_profile_name);
        let agent_dir = if let Some(p) = &profile {
            Self::profile_agent_dir(&config_root, p)
        } else {
            non_empty("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .filter(|d| {
                    pi_profile_norm
                        .as_deref()
                        .is_none_or(|p| d != &Self::profile_agent_dir(&config_root, p))
                })
                .unwrap_or_else(|| config_root.join("agent"))
        };
        let mut paths = Self::with_roots(root, agent_dir);
        paths.config_root = config_root;
        paths.profile = profile;
        paths
    }

    pub fn with_roots(root: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> RematchPaths {
        let root = root.into();
        let agent_dir = agent_dir.into();
        let config_root = infer_config_root(&agent_dir);
        RematchPaths {
            traces: root.join("traces"),
            pending: root.join("pending"),
            staging: root.join("staging"),
            drafts: root.join("drafts"),
            archive: root.join("archive"),
            predecessors: root.join("predecessors"),
            agents: root.join("agents"),
            kept_agents: root.join("kept-agents"),
            skills_shared: root.join("skills"),
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
            config_root,
            profile: None,
            agent_dir,
        }
    }

    /// `<config_root>/agent` (the default profile's agent dir, ignoring `PI_CODING_AGENT_DIR` when a profile is active).
    pub fn default_agent_dir(&self) -> PathBuf {
        self.config_root.join("agent")
    }

    /// `<config_root>/profiles/<name>/agent`.
    pub fn profile_agent_dir(config_root: &Path, profile: &str) -> PathBuf {
        config_root.join("profiles").join(profile).join("agent")
    }

    /// `<config_root>/profiles`.
    pub fn profiles_dir(&self) -> PathBuf {
        self.config_root.join("profiles")
    }

    /// Sorted valid profile names under `<config_root>/profiles/<name>` that are directories.
    pub fn list_profiles(&self) -> Vec<String> {
        let dir = self.profiles_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| normalize_profile_name(&n))
            .collect();
        names.sort();
        names.dedup();
        names
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
            &self.skills_shared,
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

/// Normalize and validate an omp profile name (`@oh-my-pi/pi-utils/src/dirs.ts`
/// `normalizeProfileName`): returns `None` for empty, whitespace, `"default"`, or
/// syntactically invalid / Windows-reserved names.
pub fn normalize_profile_name(profile: &str) -> Option<String> {
    let s = profile.trim();
    if s.is_empty() || s == "default" || s == "." || s == ".." || s.ends_with('.') || s.len() > 64 {
        return None;
    }
    let mut chars = s.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')) {
        return None;
    }
    let base = s.split('.').next().unwrap_or(s);
    if matches!(
        base.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM0" | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT0" | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    ) {
        return None;
    }
    Some(s.to_string())
}

/// Resolve the active profile from `OMP_PROFILE` and `PI_PROFILE`: `OMP_PROFILE` takes
/// precedence whenever it is set (even if empty, which selects the default profile rather
/// than falling back to `PI_PROFILE`).
pub fn resolve_profile_env(omp: Option<&str>, pi: Option<&str>) -> Option<String> {
    normalize_profile_name(omp.or(pi)?)
}

fn infer_config_root(agent_dir: &Path) -> PathBuf {
    if agent_dir.file_name().is_some_and(|n| n == "agent") {
        if let Some(parent) = agent_dir.parent() {
            if parent
                .parent()
                .and_then(|gp| gp.file_name())
                .is_some_and(|n| n == "profiles")
            {
                if let Some(root) = parent.parent().and_then(Path::parent) {
                    return root.to_path_buf();
                }
            }
            return parent.to_path_buf();
        }
    }
    agent_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent_dir.to_path_buf())
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
        assert_eq!(p.config_root, PathBuf::from("/h/.omp"));
        assert_eq!(p.profile, None);
    }

    #[test]
    fn profile_and_config_dir_resolution_precedence() {
        // 1. Default profile + PI_CODING_AGENT_DIR override wins over default ~/.omp/agent.
        let p = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("PI_CODING_AGENT_DIR", "/custom/agent"),
        ]));
        assert_eq!(p.agent_dir, PathBuf::from("/custom/agent"));
        assert_eq!(p.profile, None);
        assert_eq!(p.root, PathBuf::from("/h/.rematch"));

        // 2. Named profile (OMP_PROFILE) resolves to ~/.omp/profiles/<name>/agent and ignores PI_CODING_AGENT_DIR;
        //    REMATCH_ROOT stays ~/.rematch (not per profile).
        let p = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("OMP_PROFILE", "work"),
            ("PI_CODING_AGENT_DIR", "/custom/agent"),
        ]));
        assert_eq!(p.profile.as_deref(), Some("work"));
        assert_eq!(p.agent_dir, PathBuf::from("/h/.omp/profiles/work/agent"));
        assert_eq!(p.root, PathBuf::from("/h/.rematch"));

        // 3. OMP_PROFILE="default" or "" selects default profile, suppresses PI_PROFILE, and honors PI_CODING_AGENT_DIR.
        let p = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("OMP_PROFILE", "default"),
            ("PI_PROFILE", "work"),
            ("PI_CODING_AGENT_DIR", "/custom/agent"),
        ]));
        assert_eq!(p.profile, None);
        assert_eq!(p.agent_dir, PathBuf::from("/custom/agent"));

        let p_empty = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("OMP_PROFILE", ""),
            ("PI_PROFILE", "work"),
        ]));
        assert_eq!(p_empty.profile, None);
        assert_eq!(p_empty.agent_dir, PathBuf::from("/h/.omp/agent"));

        // 4. When OMP_PROFILE="" overrides PI_PROFILE="work", a profile-derived PI_CODING_AGENT_DIR
        //    propagated by a parent process is ignored so default mode falls back to ~/.omp/agent.
        let p_inherited = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("OMP_PROFILE", ""),
            ("PI_PROFILE", "work"),
            ("PI_CODING_AGENT_DIR", "/h/.omp/profiles/work/agent"),
        ]));
        assert_eq!(p_inherited.agent_dir, PathBuf::from("/h/.omp/agent"));

        // 5. PI_PROFILE fallback when OMP_PROFILE is unset.
        let p_pi = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("PI_PROFILE", "research"),
        ]));
        assert_eq!(p_pi.profile.as_deref(), Some("research"));
        assert_eq!(p_pi.agent_dir, PathBuf::from("/h/.omp/profiles/research/agent"));

        // 6. PI_CONFIG_DIR replaces ~/.omp for both default and named profiles.
        let p_cfg_default = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("PI_CONFIG_DIR", ".omp-alt"),
        ]));
        assert_eq!(p_cfg_default.config_root, PathBuf::from("/h/.omp-alt"));
        assert_eq!(p_cfg_default.agent_dir, PathBuf::from("/h/.omp-alt/agent"));

        let p_cfg_prof = RematchPaths::from_env(env(&[
            ("HOME", "/h"),
            ("PI_CONFIG_DIR", ".omp-alt"),
            ("OMP_PROFILE", "work"),
            ("PI_CODING_AGENT_DIR", "/ignored"),
        ]));
        assert_eq!(p_cfg_prof.config_root, PathBuf::from("/h/.omp-alt"));
        assert_eq!(p_cfg_prof.agent_dir, PathBuf::from("/h/.omp-alt/profiles/work/agent"));
        assert_eq!(p_cfg_prof.root, PathBuf::from("/h/.rematch"));
    }

    #[test]
    fn profile_name_validation_matches_omp() {
        assert_eq!(normalize_profile_name("work-1.a_b"), Some("work-1.a_b".into()));
        assert_eq!(normalize_profile_name("  work "), Some("work".into()));
        for bad in ["", "   ", "default", ".", "..", "foo.", "-bad", " Upper", "con", "nul.txt", "lpt1"] {
            assert_eq!(normalize_profile_name(bad), None, "expected {bad:?} to be rejected");
        }
    }
    #[test]
    fn off_by_env_or_marker() {
        let t = tempfile::tempdir().unwrap();
        let p = RematchPaths::with_roots(t.path().join("root"), t.path().join("agent"));
        p.ensure_dirs().unwrap();
        assert!(p.staging.is_dir() && p.blobs.is_dir() && p.agents.is_dir() && p.kept_agents.is_dir() && p.skills_shared.is_dir());
        assert!(!RematchPaths::off_by(None, &p.off_marker));
        assert!(!RematchPaths::off_by(Some("1"), &p.off_marker));
        assert!(RematchPaths::off_by(Some("0"), &p.off_marker));
        std::fs::write(&p.off_marker, "").unwrap();
        assert!(RematchPaths::off_by(Some("1"), &p.off_marker));
    }
}
