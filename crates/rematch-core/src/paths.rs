//! Root resolution (DESIGN §1, `omp://config-usage.md#profiles`, `omp://extension-loading.md`):
//! - `REMATCH_ROOT` = `$REMATCH_ROOT` or `~/.rematch` (never per profile).
//! - `config_root` = `~/.omp`, or `~/<PI_CONFIG_DIR>` when `PI_CONFIG_DIR` is set (or `<PI_CONFIG_DIR>` if absolute).
//! - Active profile = `OMP_PROFILE` when present in the environment (empty/`"default"` selects the default profile and suppresses `PI_PROFILE`), else `PI_PROFILE`; invalid or `"default"`/empty ⇒ `None`.
//! - `agent_dir` = `<config_root>/profiles/<name>/agent` when a named profile is active (`PI_CODING_AGENT_DIR` is ignored for named profiles); else `$PI_CODING_AGENT_DIR` when set and not equal to a profile-derived `<config_root>/profiles/<p>/agent`; else `<config_root>/agent`.

use std::borrow::Cow;
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
        let home = home_from(&get).unwrap_or_else(|| PathBuf::from("/"));
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

    /// `traces/<id>.jsonl` of a session, named by [`id_file_stem`].
    pub fn trace_file(&self, session_id: &str) -> PathBuf {
        self.traces.join(format!("{}.jsonl", id_file_stem(session_id)))
    }

    /// `pending/<id>.json` of a flagged session, named by [`id_file_stem`].
    pub fn pending_file(&self, session_id: &str) -> PathBuf {
        self.pending.join(format!("{}.json", id_file_stem(session_id)))
    }
}

/// The user's home: `$HOME`, else (Windows, where it is usually unset) `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    home_from(|k| std::env::var(k).ok())
}

pub(crate) fn home_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let home = non_empty("HOME");
    #[cfg(windows)]
    let home = home.or_else(|| non_empty("USERPROFILE"));
    home.map(PathBuf::from)
}

/// `std::fs::canonicalize` without Windows' verbatim `\\?\` prefix, which git, globs, link
/// targets and user-written paths never carry ([`strip_verbatim`]).
pub fn canonicalize(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    if let Some(plain) = c.to_str().and_then(strip_verbatim) {
        return Ok(PathBuf::from(plain));
    }
    Ok(c)
}

/// The plain spelling of a Windows verbatim path — `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\server\share\x` → `\\server\share\x` — when it names the same file: at most 259
/// chars (`MAX_PATH` with its NUL), and no component that plain-path parsing would alter (a
/// trailing `.` or space, a reserved device name, `/`). `None` otherwise (kept verbatim).
pub fn strip_verbatim(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    let (plain, path) = if let Some(unc) = rest.strip_prefix(r"UNC\") {
        (format!(r"\\{unc}"), unc)
    } else {
        let b = rest.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        (rest.to_string(), &rest[3..])
    };
    let altered = |c: &str| c.ends_with('.') || c.ends_with(' ') || is_reserved_name(c);
    if plain.chars().count() > 259 || path.contains('/') || path.split('\\').any(altered) {
        return None;
    }
    Some(plain)
}

/// A Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, any
/// case, with or without an extension), which no file name may take.
fn is_reserved_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    match base.len() {
        3 => ["CON", "PRN", "AUX", "NUL"].iter().any(|r| base.eq_ignore_ascii_case(r)),
        4 if base.is_ascii() => {
            let (head, n) = base.split_at(3);
            (head.eq_ignore_ascii_case("COM") || head.eq_ignore_ascii_case("LPT")) && n.as_bytes()[0].is_ascii_digit()
        }
        _ => false,
    }
}

/// The portable file stem of a logical id (a session id such as `cc:<uuid>`, `oc:<id>:r2`):
/// every byte of a char no Windows file name may hold (`<>:"/\|?*`, control chars) and of `%`
/// becomes `%XX` (uppercase hex), as does a leading `.`, a trailing `.` or space, and the
/// first char of a reserved device name ([`is_reserved_name`]). The result is one path
/// component valid on every platform; [`decode_file_stem`] inverts it. The lake names its
/// events files this way on every platform (DESIGN §19.1).
pub fn encode_file_stem(id: &str) -> Cow<'_, str> {
    let last = id.len().saturating_sub(1);
    let reserved = is_reserved_name(id);
    let escape = |i: usize, c: char| {
        matches!(c, '%' | '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            || (c as u32) < 0x20
            || (i == 0 && (c == '.' || reserved))
            || (i == last && matches!(c, '.' | ' '))
    };
    if !id.char_indices().any(|(i, c)| escape(i, c)) {
        return Cow::Borrowed(id);
    }
    let mut out = String::with_capacity(id.len() + 8);
    for (i, c) in id.char_indices() {
        if escape(i, c) {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// The id an [`encode_file_stem`] stem names. Decodes only a stem that is exactly the
/// encoding of its decoded form, so a stem written raw (a lake events file exported before the
/// codec, `cc:<uuid>`, or an id holding a literal `%41`) is returned as is and nothing is
/// decoded twice.
pub fn decode_file_stem(stem: &str) -> Cow<'_, str> {
    if !stem.contains('%') {
        return Cow::Borrowed(stem);
    }
    let (b, mut bytes, mut i) = (stem.as_bytes(), Vec::with_capacity(stem.len()), 0);
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(x)) => {
                bytes.push(x);
                i += 3;
            }
            (x, _) => {
                bytes.push(x);
                i += 1;
            }
        }
    }
    match String::from_utf8(bytes) {
        Ok(id) if encode_file_stem(&id) == stem => Cow::Owned(id),
        _ => Cow::Borrowed(stem),
    }
}

/// The file stem of a session id in this machine's `traces/`, `pending/` and adapter state
/// dirs: the id itself on Unix (the layout every existing root has), [`encode_file_stem`] on
/// Windows, where `cc:<uuid>.jsonl` would name an NTFS alternate data stream of a file `cc`.
pub fn id_file_stem(id: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        encode_file_stem(id)
    } else {
        Cow::Borrowed(id)
    }
}

/// The session id an [`id_file_stem`] names.
pub fn id_from_file_stem(stem: &str) -> Cow<'_, str> {
    if cfg!(windows) {
        decode_file_stem(stem)
    } else {
        Cow::Borrowed(stem)
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

    /// Every stem is one component Windows accepts: no reserved char, no device name, no
    /// leading dot, no trailing dot or space.
    fn windows_legal(stem: &str) -> bool {
        !stem.is_empty()
            && !stem.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20)
            && !stem.starts_with('.')
            && !stem.ends_with(['.', ' '])
            && !is_reserved_name(stem)
    }

    #[test]
    fn file_stem_codec_round_trips_every_harness_id() {
        let cases = [
            ("0198a1b2-omp", "0198a1b2-omp"),
            ("cc:11111111-2222", "cc%3A11111111-2222"),
            ("cc:sid:agent-a12", "cc%3Asid%3Aagent-a12"),
            ("codex:019a", "codex%3A019a"),
            ("pi:abc:r2", "pi%3Aabc%3Ar2"),
            ("oc:ses_1:r3", "oc%3Ases_1%3Ar3"),
            ("50%", "50%25"),
            ("a%41", "a%2541"),
            ("../up", "%2E.%2Fup"),
            ("a\\b/c", "a%5Cb%2Fc"),
            ("con", "%63on"),
            ("CON", "%43ON"),
            ("nul", "%6Eul"),
            ("Aux.json", "%41ux.json"),
            ("com1", "%63om1"),
            ("LPT1.x", "%4CPT1.x"),
            ("end.", "end%2E"),
            ("end ", "end%20"),
            ("é:ü", "é%3Aü"),
            ("tab\there", "tab%09here"),
        ];
        for (id, stem) in cases {
            assert_eq!(encode_file_stem(id), stem, "{id:?}");
            assert!(windows_legal(stem), "{stem:?}");
            assert_eq!(decode_file_stem(stem), id, "{stem:?}");
        }
        // Unchanged ids borrow.
        assert!(matches!(encode_file_stem("plain-id"), Cow::Borrowed(_)));
    }

    #[test]
    fn decode_leaves_raw_and_non_canonical_stems_alone() {
        // Lake events files exported before the codec keep the raw id as their name.
        assert_eq!(decode_file_stem("cc:abc"), "cc:abc");
        // A literal `%41` is not an encoding (`%` would be `%25`): decoded at most once.
        assert_eq!(decode_file_stem("a%41"), "a%41");
        assert_eq!(decode_file_stem("a%2541"), "a%41");
        assert_eq!(decode_file_stem("cc%3aabc"), "cc%3aabc", "lowercase hex is not what encode writes");
        assert_eq!(decode_file_stem("bad%zz"), "bad%zz");
        assert_eq!(decode_file_stem("trail%"), "trail%");
        assert_eq!(decode_file_stem("%FF%FE"), "%FF%FE", "invalid UTF-8 stays raw");
    }

    #[test]
    fn native_id_files_keep_unix_names_and_encode_on_windows() {
        let p = RematchPaths::with_roots("/r", "/a");
        let (trace, pending) = (p.trace_file("cc:abc"), p.pending_file("cc:abc"));
        if cfg!(windows) {
            assert_eq!(trace, p.traces.join("cc%3Aabc.jsonl"));
            assert_eq!(pending, p.pending.join("cc%3Aabc.json"));
            assert_eq!(id_from_file_stem("cc%3Aabc"), "cc:abc");
        } else {
            assert_eq!(trace, p.traces.join("cc:abc.jsonl"));
            assert_eq!(pending, p.pending.join("cc:abc.json"));
            assert_eq!(id_from_file_stem("cc%3Aabc"), "cc%3Aabc");
        }
        assert_eq!(p.trace_file("0198-omp"), p.traces.join("0198-omp.jsonl"));
        assert_eq!(id_from_file_stem(&id_file_stem("oc:x:r2")), "oc:x:r2");
    }

    #[test]
    fn encoded_stems_survive_a_directory_listing() {
        let t = tempfile::tempdir().unwrap();
        let ids = ["cc:a", "oc:b:r2", "pi:c", "codex:d", "50%", "aux"];
        for id in ids {
            std::fs::write(t.path().join(format!("{}.jsonl", encode_file_stem(id))), id).unwrap();
        }
        let mut back: Vec<String> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .map(|p| decode_file_stem(p.file_stem().unwrap().to_str().unwrap()).into_owned())
            .collect();
        back.sort();
        let mut want: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(back, want);
    }

    #[test]
    fn verbatim_prefix_stripped_only_when_the_plain_path_is_the_same_file() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\me\repo").as_deref(), Some(r"C:\Users\me\repo"));
        assert_eq!(strip_verbatim(r"\\?\UNC\server\share\repo").as_deref(), Some(r"\\server\share\repo"));
        assert_eq!(strip_verbatim(r"C:\Users\me"), None, "not verbatim");
        assert_eq!(strip_verbatim(r"\\?\Volume{1234}\x"), None, "no drive or UNC spelling");
        assert_eq!(strip_verbatim(r"\\?\C:\dir\trailing."), None);
        assert_eq!(strip_verbatim(r"\\?\C:\dir\nul.txt"), None);
        assert_eq!(strip_verbatim(r"\\?\C:\a/b"), None);
        let long = format!(r"\\?\C:\{}", "x".repeat(300));
        assert_eq!(strip_verbatim(&long), None, "beyond MAX_PATH only the verbatim form works");
    }

    #[test]
    fn home_is_home_then_userprofile_on_windows() {
        assert_eq!(home_from(env(&[("HOME", "/h"), ("USERPROFILE", r"C:\Users\u")])), Some(PathBuf::from("/h")));
        let profile_only = home_from(env(&[("USERPROFILE", r"C:\Users\u")]));
        assert_eq!(profile_only, cfg!(windows).then(|| PathBuf::from(r"C:\Users\u")));
        assert_eq!(home_from(env(&[("HOME", " ")])), None);
    }
}
