//! Fast compiler/type/lint check detection, scope narrowing, diagnostic parsing,
//! impact symbol extraction, and compact context rendering (DESIGN §10, §16).
//!
//! Pure functions live here (`rematch-core` spawns no processes); process execution,
//! git diff/grep impact collection, and the `rematch check` CLI live in `crates/rematch/src/check.rs`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_CHECK_TIMEOUT_SECS: u32 = 30;
pub const DEFAULT_CHECK_BUDGET_BYTES: u32 = 4096;
pub const IMPACT_MAX_LINES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Cargo,
    Tsc,
    Ruff,
    Mypy,
    Pyright,
    Go,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Cargo => "cargo",
            CheckKind::Tsc => "tsc",
            CheckKind::Ruff => "ruff",
            CheckKind::Mypy => "mypy",
            CheckKind::Pyright => "pyright",
            CheckKind::Go => "go",
        }
    }

    /// Unambiguous human label for the check command (`cargo check`, not `cargo`).
    pub fn pass_label(self) -> &'static str {
        match self {
            CheckKind::Cargo => "cargo check",
            CheckKind::Tsc => "tsc",
            CheckKind::Ruff => "ruff check",
            CheckKind::Mypy => "mypy",
            CheckKind::Pyright => "pyright",
            CheckKind::Go => "go vet",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub kind: CheckKind,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactRef {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CheckKind>,
    #[serde(default)]
    pub command: Vec<String>,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impact: Vec<ImpactRef>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
}

/// Process-local detection cache keyed by repo top path.
static DETECT_CACHE: LazyLock<Mutex<BTreeMap<PathBuf, Option<CheckSpec>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Clear the detection cache (used in tests when a temp repo changes).
pub fn clear_detect_cache() {
    if let Ok(mut guard) = DETECT_CACHE.lock() {
        guard.clear();
    }
}

/// Walk ancestors from `cwd` to find a git worktree top or project root; falls back to `cwd`.
pub fn find_repo_top(cwd: &Path) -> PathBuf {
    let canon = crate::paths::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for a in canon.ancestors() {
        if a.join(".git").exists() {
            return a.to_path_buf();
        }
    }
    for a in canon.ancestors() {
        if a.join("Cargo.toml").is_file()
            || a.join("tsconfig.json").is_file()
            || a.join("pyproject.toml").is_file()
            || a.join("ruff.toml").is_file()
            || a.join(".ruff.toml").is_file()
            || a.join("pyrightconfig.json").is_file()
            || a.join("go.mod").is_file()
            || a.join("package.json").is_file()
        {
            return a.to_path_buf();
        }
    }
    canon
}

/// Detect the primary fast check for `repo_top` (cached per repo top).
pub fn detect(repo_top: &Path) -> Option<CheckSpec> {
    let key = crate::paths::canonicalize(repo_top).unwrap_or_else(|_| repo_top.to_path_buf());
    if let Ok(guard) = DETECT_CACHE.lock() {
        if let Some(cached) = guard.get(&key) {
            // Re-verify the root marker still exists in case a temp dir was reused.
            if cached.as_ref().is_none_or(|s| s.cwd.is_dir()) {
                return cached.clone();
            }
        }
    }
    let spec = detect_uncached(&key);
    if let Ok(mut guard) = DETECT_CACHE.lock() {
        guard.insert(key, spec.clone());
    }
    spec
}

/// Walk ancestors from `p` (or its parent directory if `p` is a file) to find the nearest git
/// worktree top or project root.
pub fn find_repo_top_for_file(p: &Path, fallback_cwd: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { fallback_cwd.join(p) };
    let start = abs
        .ancestors()
        .find(|a| a.is_dir())
        .or_else(|| abs.parent())
        .unwrap_or(fallback_cwd);
    find_repo_top(start)
}

/// Detect a check appropriate for `files` (resolving the project/workspace root from the edited
/// files first when they live in a different repository than `repo_top`).
pub fn detect_for_files(repo_top: &Path, files: &[PathBuf]) -> Option<CheckSpec> {
    let fallback_top = crate::paths::canonicalize(repo_top).unwrap_or_else(|_| repo_top.to_path_buf());
    let top = files
        .first()
        .map(|f| find_repo_top_for_file(f, &fallback_top))
        .unwrap_or_else(|| fallback_top.clone());
    if !files.is_empty() {
        let exts: HashSet<String> = files
            .iter()
            .filter_map(|f| f.extension().and_then(|e| e.to_str()))
            .map(|e| e.to_ascii_lowercase())
            .collect();
        if exts.contains("rs") {
            if let Some(s) = detect_cargo(&top, files) {
                return Some(s);
            }
        }
        if exts.iter().any(|e| matches!(e.as_str(), "ts" | "tsx" | "mts" | "cts")) {
            if let Some(s) = detect_tsc(&top, files) {
                return Some(s);
            }
        }
        if exts.iter().any(|e| matches!(e.as_str(), "py" | "pyi")) {
            if let Some(s) = detect_python(&top) {
                return Some(s);
            }
        }
        if exts.contains("go") {
            if let Some(s) = detect_go(&top, files) {
                return Some(s);
            }
        }
    }
    detect(&top)
}

fn detect_uncached(repo_top: &Path) -> Option<CheckSpec> {
    if let Some(s) = detect_cargo(repo_top, &[]) {
        return Some(s);
    }
    if let Some(s) = detect_tsc(repo_top, &[]) {
        return Some(s);
    }
    if let Some(s) = detect_python(repo_top) {
        return Some(s);
    }
    if let Some(s) = detect_go(repo_top, &[]) {
        return Some(s);
    }
    None
}

/// Nearest Cargo workspace root (`[workspace]` in `Cargo.toml`) or package root above `dir`.
fn find_cargo_root(dir: &Path, stop_at: Option<&Path>) -> Option<PathBuf> {
    let mut pkg_root: Option<PathBuf> = None;
    for a in dir.ancestors() {
        let manifest = a.join("Cargo.toml");
        if manifest.is_file() {
            if std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .is_some_and(|t| t.contains_key("workspace"))
            {
                return Some(a.to_path_buf());
            }
            if pkg_root.is_none() {
                pkg_root = Some(a.to_path_buf());
            }
        }
        if stop_at.is_some_and(|s| a == s) {
            break;
        }
    }
    pkg_root
}

fn cargo_spec(cwd: PathBuf) -> CheckSpec {
    CheckSpec {
        kind: CheckKind::Cargo,
        argv: vec![
            "cargo".into(),
            "check".into(),
            "--message-format=json".into(),
            "--workspace".into(),
            "--all-targets".into(),
            "--quiet".into(),
        ],
        cwd,
    }
}

fn detect_cargo(repo_top: &Path, files: &[PathBuf]) -> Option<CheckSpec> {
    // Prefer the edited files' own Cargo workspace/package first (even when the session cwd is in
    // another repository or a parent directory).
    for f in files {
        let abs = if f.is_absolute() { f.clone() } else { repo_top.join(f) };
        let file_top = find_repo_top_for_file(&abs, repo_top);
        let start = abs.parent().unwrap_or(&abs);
        if let Some(root) = find_cargo_root(start, Some(&file_top)).or_else(|| find_cargo_root(start, None)) {
            return Some(cargo_spec(root));
        }
    }
    if repo_top.join("Cargo.toml").is_file() {
        return Some(cargo_spec(repo_top.to_path_buf()));
    }
    None
}

fn has_typescript_dep(pkg_json: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pkg_json) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if v.get(section).and_then(Value::as_object).is_some_and(|m| m.contains_key("typescript")) {
            return true;
        }
    }
    false
}

fn local_tsc_bin(dir: &Path) -> Option<PathBuf> {
    let bin_name = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
    for a in dir.ancestors() {
        let p = a.join("node_modules").join(".bin").join(bin_name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn find_program_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let candidates: Vec<String> = if cfg!(windows) && !name.contains('.') {
        vec![format!("{name}.exe"), format!("{name}.cmd"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&path_var) {
        for c in &candidates {
            let p = dir.join(c);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn tsc_base_argv(project_dir: &Path, repo_top: &Path) -> Option<Vec<String>> {
    if let Some(bin) = local_tsc_bin(project_dir).or_else(|| local_tsc_bin(repo_top)) {
        return Some(vec![bin.to_string_lossy().into_owned(), "--noEmit".into(), "--pretty".into(), "false".into()]);
    }
    let has_dep = has_typescript_dep(&project_dir.join("package.json"))
        || has_typescript_dep(&repo_top.join("package.json"));
    if !has_dep {
        return None;
    }
    if find_program_in_path("bunx").is_some() {
        Some(vec!["bunx".into(), "tsc".into(), "--noEmit".into(), "--pretty".into(), "false".into()])
    } else if find_program_in_path("npx").is_some() {
        Some(vec!["npx".into(), "--no-install".into(), "tsc".into(), "--noEmit".into(), "--pretty".into(), "false".into()])
    } else {
        Some(vec!["tsc".into(), "--noEmit".into(), "--pretty".into(), "false".into()])
    }
}

fn detect_tsc(repo_top: &Path, files: &[PathBuf]) -> Option<CheckSpec> {
    let tsconfig = repo_top.join("tsconfig.json");
    if tsconfig.is_file() {
        let mut argv = tsc_base_argv(repo_top, repo_top)?;
        argv.push("-p".into());
        argv.push("tsconfig.json".into());
        return Some(CheckSpec { kind: CheckKind::Tsc, argv, cwd: repo_top.to_path_buf() });
    }
    for f in files {
        let abs = if f.is_absolute() { f.clone() } else { repo_top.join(f) };
        for a in abs.ancestors() {
            if a.join("tsconfig.json").is_file() {
                let mut argv = tsc_base_argv(a, repo_top)?;
                argv.push("-p".into());
                argv.push(a.join("tsconfig.json").to_string_lossy().into_owned());
                return Some(CheckSpec { kind: CheckKind::Tsc, argv, cwd: repo_top.to_path_buf() });
            }
            if a == repo_top {
                break;
            }
        }
    }
    None
}

fn detect_python(repo_top: &Path) -> Option<CheckSpec> {
    let ruff_cfg = repo_top.join("ruff.toml").is_file() || repo_top.join(".ruff.toml").is_file();
    let pyright_cfg = repo_top.join("pyrightconfig.json").is_file();
    let pyproject = repo_top.join("pyproject.toml");
    let py_table = std::fs::read_to_string(&pyproject)
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok());
    let has_tool = |name: &str| {
        py_table
            .as_ref()
            .and_then(|t| t.get("tool"))
            .and_then(|v| v.as_table())
            .is_some_and(|m| m.contains_key(name))
    };

    if (pyright_cfg || has_tool("pyright")) && (find_program_in_path("pyright").is_some() || local_venv_bin(repo_top, "pyright").is_some()) {
        let bin = local_venv_bin(repo_top, "pyright")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pyright".into());
        return Some(CheckSpec {
            kind: CheckKind::Pyright,
            argv: vec![bin, "--outputjson".into()],
            cwd: repo_top.to_path_buf(),
        });
    }
    if (has_tool("mypy") || repo_top.join("mypy.ini").is_file() || repo_top.join(".mypy.ini").is_file())
        && (find_program_in_path("mypy").is_some() || local_venv_bin(repo_top, "mypy").is_some())
    {
        let bin = local_venv_bin(repo_top, "mypy")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mypy".into());
        return Some(CheckSpec {
            kind: CheckKind::Mypy,
            argv: vec![bin, "--no-color-output".into(), "--no-error-summary".into(), "--show-column-numbers".into()],
            cwd: repo_top.to_path_buf(),
        });
    }
    if ruff_cfg || has_tool("ruff") || py_table.is_some() {
        let bin = local_venv_bin(repo_top, "ruff")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ruff".into());
        if ruff_cfg || has_tool("ruff") || find_program_in_path("ruff").is_some() || local_venv_bin(repo_top, "ruff").is_some() {
            return Some(CheckSpec {
                kind: CheckKind::Ruff,
                argv: vec![bin, "check".into(), "--output-format=json".into()],
                cwd: repo_top.to_path_buf(),
            });
        }
    }
    if pyright_cfg || has_tool("pyright") {
        return Some(CheckSpec {
            kind: CheckKind::Pyright,
            argv: vec!["pyright".into(), "--outputjson".into()],
            cwd: repo_top.to_path_buf(),
        });
    }
    if has_tool("mypy") {
        return Some(CheckSpec {
            kind: CheckKind::Mypy,
            argv: vec!["mypy".into(), "--no-color-output".into(), "--no-error-summary".into(), "--show-column-numbers".into()],
            cwd: repo_top.to_path_buf(),
        });
    }
    None
}

fn local_venv_bin(repo_top: &Path, name: &str) -> Option<PathBuf> {
    let sub = if cfg!(windows) { "Scripts" } else { "bin" };
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    for venv in [".venv", "venv"] {
        let p = repo_top.join(venv).join(sub).join(&exe);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn detect_go(repo_top: &Path, files: &[PathBuf]) -> Option<CheckSpec> {
    if repo_top.join("go.mod").is_file() {
        return Some(CheckSpec {
            kind: CheckKind::Go,
            argv: vec!["go".into(), "vet".into(), "./...".into()],
            cwd: repo_top.to_path_buf(),
        });
    }
    for f in files {
        let abs = if f.is_absolute() { f.clone() } else { repo_top.join(f) };
        for a in abs.ancestors() {
            if a.join("go.mod").is_file() {
                return Some(CheckSpec {
                    kind: CheckKind::Go,
                    argv: vec!["go".into(), "vet".into(), "./...".into()],
                    cwd: a.to_path_buf(),
                });
            }
            if a == repo_top {
                break;
            }
        }
    }
    None
}

/// Package name from a `Cargo.toml` file (`[package].name`).
pub fn cargo_package_name(cargo_toml: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cargo_toml).ok()?;
    let table = toml::from_str::<toml::Table>(&text).ok()?;
    table
        .get("package")?
        .as_table()?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

impl CheckSpec {
    /// Narrow `argv` for the given changed files (e.g. `cargo check -p <crate>`,
    /// `tsc -p <nearest tsconfig.json>`, `ruff check <files...>`, `go vet <pkg>`).
    pub fn scope_for(&self, files: &[PathBuf]) -> CheckSpec {
        if files.is_empty() {
            return self.clone();
        }
        let abs_files: Vec<PathBuf> = files
            .iter()
            .map(|f| if f.is_absolute() { f.clone() } else { self.cwd.join(f) })
            .collect();

        match self.kind {
            // The whole workspace and every target: an edit breaks dependent crates and tests,
            // and incremental checking leaves the crates it cannot affect fresh, so narrowing to
            // `-p <crate>` saves almost nothing and misses exactly those breakages.
            CheckKind::Cargo => self.clone(),
            CheckKind::Tsc => {
                // Find the nearest tsconfig.json for the changed TS/JS files.
                let mut best_tsconfig: Option<PathBuf> = None;
                for f in &abs_files {
                    for a in f.ancestors() {
                        let candidate = a.join("tsconfig.json");
                        if candidate.is_file() {
                            best_tsconfig = Some(candidate);
                            break;
                        }
                        if a == self.cwd {
                            break;
                        }
                    }
                    if best_tsconfig.is_some() {
                        break;
                    }
                }
                let mut argv = self.argv.clone();
                if let Some(ts) = best_tsconfig {
                    let rel = ts
                        .strip_prefix(&self.cwd)
                        .unwrap_or(&ts)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if let Some(idx) = argv.iter().position(|a| a == "-p" || a == "--project") {
                        if idx + 1 < argv.len() {
                            argv[idx + 1] = rel;
                        }
                    } else {
                        argv.push("-p".into());
                        argv.push(rel);
                    }
                }
                CheckSpec { kind: self.kind, argv, cwd: self.cwd.clone() }
            }
            CheckKind::Ruff | CheckKind::Mypy | CheckKind::Pyright => {
                let py_files: Vec<String> = abs_files
                    .iter()
                    .filter(|f| {
                        f.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "py" | "pyi"))
                    })
                    .map(|f| {
                        f.strip_prefix(&self.cwd)
                            .unwrap_or(f)
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .collect();
                let mut argv = self.argv.clone();
                for pf in py_files {
                    if !argv.contains(&pf) {
                        argv.push(pf);
                    }
                }
                CheckSpec { kind: self.kind, argv, cwd: self.cwd.clone() }
            }
            CheckKind::Go => {
                let mut dirs = Vec::new();
                for f in &abs_files {
                    if f.extension().and_then(|e| e.to_str()) == Some("go") {
                        if let Some(parent) = f.parent() {
                            let rel = parent
                                .strip_prefix(&self.cwd)
                                .unwrap_or(parent)
                                .to_string_lossy()
                                .replace('\\', "/");
                            let pkg = if rel.is_empty() || rel == "." {
                                ".".to_string()
                            } else if rel.starts_with('.') {
                                rel
                            } else {
                                format!("./{rel}")
                            };
                            if !dirs.contains(&pkg) {
                                dirs.push(pkg);
                            }
                        }
                    }
                }
                let mut argv = self.argv.clone();
                if !dirs.is_empty() {
                    if let Some(pos) = argv.iter().position(|a| a == "./...") {
                        argv.splice(pos..=pos, dirs);
                    }
                }
                CheckSpec { kind: self.kind, argv, cwd: self.cwd.clone() }
            }
        }
    }
}

fn normalize_rel_path(raw: &str, cwd: &Path) -> String {
    let p = Path::new(raw);
    let rel = if p.is_absolute() {
        relative_to(p, cwd).unwrap_or_else(|| p.to_path_buf()).to_string_lossy().into_owned()
    } else {
        raw.to_string()
    };
    rel.replace('\\', "/")
}

/// `p` relative to `base`, also when either is reached through a symlink (`/tmp` →
/// `/private/tmp` on macOS, a symlinked home). The nearest existing ancestor of `p` is
/// canonicalized, since an edited file may be gone.
fn relative_to(p: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(r) = p.strip_prefix(base) {
        return Some(r.to_path_buf());
    }
    let base = std::fs::canonicalize(base).ok()?;
    for a in p.ancestors() {
        if let Ok(real) = std::fs::canonicalize(a) {
            return real.join(p.strip_prefix(a).ok()?).strip_prefix(&base).ok().map(Path::to_path_buf);
        }
    }
    None
}

/// Parse `cargo check --message-format=json` stdout into diagnostics (errors first, with
/// primary spans and child `help:` / suggested replacements extracted).
pub fn parse_cargo_json(stdout: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let level = msg.get("level").and_then(Value::as_str).unwrap_or("");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let text = msg.get("message").and_then(Value::as_str).unwrap_or("").trim();
        if text.is_empty() || text.starts_with("aborting due to") || text.starts_with("For more information about") {
            continue;
        }
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let spans = msg.get("spans").and_then(Value::as_array);
        let primary = spans
            .and_then(|arr| {
                arr.iter()
                    .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
                    .or_else(|| arr.first())
            });
        let Some(span) = primary else { continue };
        let file = normalize_rel_path(span.get("file_name").and_then(Value::as_str).unwrap_or(""), cwd);
        let line_num = span.get("line_start").and_then(Value::as_u64).unwrap_or(1) as u32;
        let col_num = span.get("column_start").and_then(Value::as_u64).unwrap_or(1) as u32;

        // Extract help + suggested_replacement from children or primary span label.
        let mut helps = Vec::new();
        if let Some(children) = msg.get("children").and_then(Value::as_array) {
            for child in children {
                let c_level = child.get("level").and_then(Value::as_str).unwrap_or("");
                if c_level != "help" && c_level != "note" {
                    continue;
                }
                let c_msg = child.get("message").and_then(Value::as_str).unwrap_or("").trim();
                let mut replacement = None;
                if let Some(c_spans) = child.get("spans").and_then(Value::as_array) {
                    for cs in c_spans {
                        if let Some(rep) = cs.get("suggested_replacement").and_then(Value::as_str) {
                            let rep = rep.trim();
                            if !rep.is_empty() {
                                replacement = Some(rep.to_string());
                                break;
                            }
                        }
                    }
                }
                if c_level == "help" || replacement.is_some() {
                    let h = match (c_msg.is_empty(), replacement) {
                        (false, Some(r)) => format!("{c_msg}: `{r}`"),
                        (true, Some(r)) => format!("replace with `{r}`"),
                        (false, None) => c_msg.to_string(),
                        (true, None) => continue,
                    };
                    if !helps.contains(&h) {
                        helps.push(h);
                    }
                }
            }
        }
        let help = (!helps.is_empty()).then(|| helps.join("; "));
        let key = (file.clone(), line_num, col_num, text.to_string());
        if seen.insert(key) {
            out.push(Diagnostic {
                file,
                line: line_num,
                col: col_num,
                severity: level.to_string(),
                code,
                message: text.to_string(),
                help,
            });
        }
    }
    sort_diagnostics(&mut out);
    out
}

static TSC_DIAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?)\((\d+),(\d+)\):\s*(error|warning)\s+(TS\d+):\s*(.+)$").expect("static regex")
});
static TSC_COLON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+):(\d+)\s*-\s*(error|warning)\s+(TS\d+):\s*(.+)$").expect("static regex")
});

/// Parse `tsc --noEmit` output into diagnostics.
pub fn parse_tsc_output(output: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        let caps = TSC_DIAG_RE.captures(line).or_else(|| TSC_COLON_RE.captures(line));
        let Some(c) = caps else { continue };
        let file = normalize_rel_path(&c[1], cwd);
        let line_num = c[2].parse::<u32>().unwrap_or(1);
        let col_num = c[3].parse::<u32>().unwrap_or(1);
        let severity = c[4].to_string();
        let code = Some(c[5].to_string());
        let message = c[6].trim().to_string();
        let key = (file.clone(), line_num, col_num, message.clone());
        if seen.insert(key) {
            out.push(Diagnostic {
                file,
                line: line_num,
                col: col_num,
                severity,
                code,
                message,
                help: None,
            });
        }
    }
    sort_diagnostics(&mut out);
    out
}

/// Parse `ruff check --output-format=json` stdout into diagnostics.
pub fn parse_ruff_json(stdout: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(stdout.trim()) else {
        return out;
    };
    for item in arr {
        let file = normalize_rel_path(item.get("filename").and_then(Value::as_str).unwrap_or(""), cwd);
        let line_num = item
            .pointer("/location/row")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        let col_num = item
            .pointer("/location/column")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        let code = item
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if message.is_empty() {
            continue;
        }
        let help = item
            .get("fix")
            .and_then(|f| {
                let msg = f.get("message").and_then(Value::as_str).unwrap_or("").trim();
                let edits = f
                    .get("edits")
                    .and_then(Value::as_array)
                    .and_then(|e| e.first())
                    .and_then(|e| e.get("content"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                match (msg.is_empty(), edits) {
                    (false, Some(rep)) => Some(format!("{msg}: `{rep}`")),
                    (false, None) => Some(msg.to_string()),
                    (true, Some(rep)) => Some(format!("replace with `{rep}`")),
                    (true, None) => None,
                }
            });
        out.push(Diagnostic {
            file,
            line: line_num,
            col: col_num,
            severity: "error".into(),
            code,
            message,
            help,
        });
    }
    sort_diagnostics(&mut out);
    out
}

/// Parse `pyright --outputjson` stdout into diagnostics.
pub fn parse_pyright_json(stdout: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) else {
        return out;
    };
    let Some(diags) = v.get("generalDiagnostics").and_then(Value::as_array) else {
        return out;
    };
    for d in diags {
        let sev = d.get("severity").and_then(Value::as_str).unwrap_or("error");
        if !matches!(sev, "error" | "warning") {
            continue;
        }
        let file = normalize_rel_path(d.get("file").and_then(Value::as_str).unwrap_or(""), cwd);
        let line_num = d
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .map(|n| n as u32 + 1)
            .unwrap_or(1);
        let col_num = d
            .pointer("/range/start/character")
            .and_then(Value::as_u64)
            .map(|n| n as u32 + 1)
            .unwrap_or(1);
        let code = d.get("rule").and_then(Value::as_str).map(str::to_string);
        let message = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ")
            .trim()
            .to_string();
        if !message.is_empty() {
            out.push(Diagnostic {
                file,
                line: line_num,
                col: col_num,
                severity: sev.to_string(),
                code,
                message,
                help: None,
            });
        }
    }
    sort_diagnostics(&mut out);
    out
}

static MYPY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):(\d+):(?:(\d+):)?\s*(error|warning|note):\s*(.+?)(?:\s+\[([a-zA-Z0-9_-]+)\])?$")
        .expect("static regex")
});

/// Parse `mypy` line output into diagnostics (attaches `note:` lines as `help` on the preceding diagnostic).
pub fn parse_mypy_output(output: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        let Some(c) = MYPY_RE.captures(line) else { continue };
        let file = normalize_rel_path(&c[1], cwd);
        let line_num = c[2].parse::<u32>().unwrap_or(1);
        let col_num = c.get(3).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(1);
        let level = &c[4];
        let message = c[5].trim().to_string();
        let code = c.get(6).map(|m| m.as_str().to_string());
        if level == "note" {
            if let Some(last) = out.last_mut() {
                if last.file == file && last.line == line_num {
                    last.help = Some(match &last.help {
                        Some(prev) => format!("{prev}; {message}"),
                        None => message,
                    });
                }
            }
            continue;
        }
        out.push(Diagnostic {
            file,
            line: line_num,
            col: col_num,
            severity: level.to_string(),
            code,
            message,
            help: None,
        });
    }
    sort_diagnostics(&mut out);
    out
}

static GO_DIAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:]+\.go):(\d+):(?:(\d+):)?\s*(.+)$").expect("static regex")
});

/// Parse `go vet` / `go build` stderr/stdout into diagnostics.
pub fn parse_go_output(output: &str, cwd: &Path) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some(c) = GO_DIAG_RE.captures(line) else { continue };
        let file = normalize_rel_path(&c[1], cwd);
        let line_num = c[2].parse::<u32>().unwrap_or(1);
        let col_num = c.get(3).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(1);
        let message = c[4].trim().to_string();
        let key = (file.clone(), line_num, col_num, message.clone());
        if seen.insert(key) {
            out.push(Diagnostic {
                file,
                line: line_num,
                col: col_num,
                severity: "error".into(),
                code: None,
                message,
                help: None,
            });
        }
    }
    sort_diagnostics(&mut out);
    out
}

fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        let a_err = a.severity == "error";
        let b_err = b.severity == "error";
        b_err
            .cmp(&a_err)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.col.cmp(&b.col))
    });
}

static DEF_SIG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|\b)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|const\s+|unsafe\s+|export\s+|default\s+|declare\s+)*(?:fn|struct|enum|trait|type|class|def|interface|function)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("static regex")
});

static EXPORT_CONST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*export\s+(?:const|let|var)\s+([A-Za-z_][A-Za-z0-9_]*)").expect("static regex")
});

const COMMON_IGNORE_NAMES: &[&str] = &[
    "main", "new", "default", "from", "into", "drop", "clone", "fmt", "eq", "cmp", "hash",
    "partial_cmp", "index", "deref", "next", "build", "run", "test", "setup", "init", "__init__",
];

/// Extract definition names (`fn`, `struct`, `enum`, `trait`, `type`, `class`, `def`,
/// `interface`, `function`, `export const`) whose signature or name changed in a unified
/// `git diff` output.
///
/// Only symbols that appear on a removed (`-`) or added (`+`) definition header line where
/// the exact header line was not identically present on the other side of the diff are
/// returned.
pub fn extract_changed_definitions(diff_text: &str) -> Vec<String> {
    let mut removed_lines = HashSet::new();
    let mut added_lines = HashSet::new();
    let mut candidates = Vec::new();

    for raw in diff_text.lines() {
        if raw.starts_with("---") || raw.starts_with("+++") || raw.starts_with("@@") {
            continue;
        }
        let Some((sign, rest)) = raw.strip_prefix('-').map(|r| ('-', r)).or_else(|| raw.strip_prefix('+').map(|r| ('+', r))) else {
            continue;
        };
        let norm = rest.trim().trim_end_matches('{').trim();
        let sym = DEF_SIG_RE
            .captures(norm)
            .or_else(|| EXPORT_CONST_RE.captures(norm))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        let Some(name) = sym else { continue };
        if name.len() < 3 || COMMON_IGNORE_NAMES.contains(&name.as_str()) {
            continue;
        }
        if sign == '-' {
            removed_lines.insert(norm.to_string());
        } else {
            added_lines.insert(norm.to_string());
        }
        candidates.push((name, norm.to_string()));
    }

    let mut out = Vec::new();
    for (name, norm) in candidates {
        // Pure movement of the exact same signature line is not a signature/name change.
        if removed_lines.contains(&norm) && added_lines.contains(&norm) {
            continue;
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Filter and merge impact references with diagnostics:
/// - skip files edited this turn,
/// - skip `target/`, `node_modules/`, `vendor/`, `.git/`, `dist/`, `build/`,
/// - diagnostics win when both point at the same `(file, line)`,
/// - cap at `max_lines` (default 20).
pub fn merge_impact_refs(
    raw_refs: Vec<ImpactRef>,
    edited_files: &[PathBuf],
    diagnostics: &[Diagnostic],
    cwd: &Path,
    max_lines: usize,
) -> Vec<ImpactRef> {
    let edited_rel: HashSet<String> = edited_files
        .iter()
        .map(|p| normalize_rel_path(&p.to_string_lossy(), cwd))
        .collect();
    let diag_locs: HashSet<(String, u32)> = diagnostics
        .iter()
        .map(|d| (d.file.clone(), d.line))
        .collect();
    let mut seen_locs = HashSet::new();
    let mut out = Vec::new();
    for r in raw_refs {
        let file = normalize_rel_path(&r.file, cwd);
        if edited_rel.contains(&file) {
            continue;
        }
        if is_ignored_impact_path(&file) {
            continue;
        }
        if diag_locs.contains(&(file.clone(), r.line)) {
            continue;
        }
        if !seen_locs.insert((file.clone(), r.line)) {
            continue;
        }
        out.push(ImpactRef {
            symbol: r.symbol,
            file,
            line: r.line,
            text: r.text.trim().to_string(),
        });
        if out.len() >= max_lines {
            break;
        }
    }
    out
}

pub fn is_ignored_impact_path(rel_path: &str) -> bool {
    rel_path.split('/').any(|seg| {
        matches!(
            seg,
            "target" | "node_modules" | "vendor" | ".git" | "dist" | ".next" | "__pycache__" | ".venv" | "venv"
        )
    })
}

/// Format a `CheckReport` into the compact hidden context message injected after edits.
/// Returns `(rendered_message, truncated)`.
///
/// Order:
/// 1. Timeout line if timed out (`[rematch fast-check] <kind> timed out after Xs`)
/// 2. Errors/warnings first (`file:line:col [code] message` + `help: ...`)
/// 3. Impact references in unedited files (`impact (<symbol>): file:line: text`)
/// 4. One-line `✓ <kind> passed in X.Xs` when clean (`ok && errors == 0`)
///
/// Bounded to `budget_bytes` (e.g. 4096) with a truncation note.
pub fn render_report(report: &CheckReport, budget_bytes: usize) -> (String, bool) {
    let budget = budget_bytes.max(256);
    let kind_name = report.kind.map(CheckKind::as_str).unwrap_or("check");
    let pass_label = report.kind.map(CheckKind::pass_label).unwrap_or("check");
    let secs = report.duration_ms as f64 / 1000.0;
    let mut lines = Vec::new();
    let errors: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == "error")
        .collect();

    if report.timed_out {
        lines.push(format!("[rematch fast-check] {kind_name} timed out after {secs:.1}s"));
    } else if !errors.is_empty() || !report.ok {
        let count = if errors.is_empty() { report.diagnostics.len() } else { errors.len() };
        lines.push(format!(
            "[rematch fast-check] {kind_name}: {count} error(s) in {secs:.1}s"
        ));
        let to_show: &[&Diagnostic] = if !errors.is_empty() {
            &errors
        } else {
            // If command failed with only warnings/notes parsed, show diagnostics.
            &[]
        };
        let iter: Box<dyn Iterator<Item = &Diagnostic>> = if !to_show.is_empty() {
            Box::new(to_show.iter().copied())
        } else {
            Box::new(report.diagnostics.iter())
        };
        for d in iter {
            let code_str = d
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            let mut line = format!("{}:{}:{}{} {}", d.file, d.line, d.col, code_str, d.message);
            if let Some(h) = d.help.as_deref().filter(|h| !h.is_empty()) {
                line.push_str(&format!("\n  help: {h}"));
            }
            lines.push(line);
        }
    }

    if !report.impact.is_empty() {
        lines.push("impact (references in unedited files):".to_string());
        for r in &report.impact {
            let snippet = if r.text.chars().count() > 140 {
                let head: String = r.text.chars().take(140).collect();
                format!("{head}…")
            } else {
                r.text.clone()
            };
            lines.push(format!("  {}:{} ({}): {}", r.file, r.line, r.symbol, snippet));
        }
    }

    if !report.timed_out && report.ok && errors.is_empty() {
        lines.push(format!("✓ {pass_label} passed in {secs:.1}s"));
    }

    let full = lines.join("\n");
    if full.len() <= budget {
        return (full, false);
    }

    let mut out = String::new();
    let mut omitted = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        let sep = if out.is_empty() { "" } else { "\n" };
        let reserve = 64; // room for truncation footer
        if out.len() + sep.len() + line.len() + reserve > budget {
            omitted = lines.len().saturating_sub(idx);
            break;
        }
        out.push_str(sep);
        out.push_str(line);
    }
    let note = format!("\n…[rematch fast-check: truncated +{omitted} line(s) to fit {budget}B budget]");
    out.push_str(&note);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cargo_json_extracts_primary_span_and_help_suggestion() {
        let sample = r#"{"reason":"compiler-message","package_id":"demo 0.1.0","message":{"rendered":"error[E0308]: mismatched types","children":[{"children":[],"code":null,"level":"help","message":"try using a conversion method","rendered":null,"spans":[{"byte_end":42,"byte_start":35,"column_end":20,"column_start":13,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":4,"line_start":4,"suggested_replacement":"x.to_string()","suggestion_applicability":"MachineApplicable","text":[]}]}],"code":{"code":"E0308","explanation":null},"level":"error","message":"mismatched types: expected `String`, found `&str`","spans":[{"byte_end":42,"byte_start":35,"column_end":20,"column_start":13,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":"expected `String`, found `&str`","line_end":4,"line_start":4,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
        let diags = parse_cargo_json(sample, Path::new("/repo"));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "src/lib.rs");
        assert_eq!(diags[0].line, 4);
        assert_eq!(diags[0].col, 13);
        assert_eq!(diags[0].code.as_deref(), Some("E0308"));
        assert_eq!(diags[0].message, "mismatched types: expected `String`, found `&str`");
        assert_eq!(
            diags[0].help.as_deref(),
            Some("try using a conversion method: `x.to_string()`")
        );
    }

    #[test]
    fn parse_tsc_and_ruff_outputs() {
        let tsc_out = "src/index.ts(12,5): error TS2322: Type 'number' is not assignable to type 'string'.\n\
                       src/util.ts:7:3 - error TS2304: Cannot find name 'missingVar'.\n";
        let diags = parse_tsc_output(tsc_out, Path::new("/repo"));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].file, "src/index.ts");
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].col, 5);
        assert_eq!(diags[0].code.as_deref(), Some("TS2322"));
        assert_eq!(diags[1].file, "src/util.ts");
        assert_eq!(diags[1].code.as_deref(), Some("TS2304"));

        // An absolute path on this platform (`/repo` is not absolute on Windows).
        let repo = std::env::temp_dir().join("repo");
        let ruff_out = serde_json::json!([{"code": "F401", "filename": repo.join("app").join("main.py"),
            "location": {"column": 8, "row": 3}, "message": "`os` imported but unused",
            "fix": {"applicability": "safe", "edits": [{"content": "", "end_location": {"column": 1, "row": 4}, "location": {"column": 1, "row": 3}}], "message": "Remove unused import: `os`"}}])
        .to_string();
        let r_diags = parse_ruff_json(&ruff_out, &repo);
        assert_eq!(r_diags.len(), 1);
        assert_eq!(r_diags[0].file, "app/main.py");
        assert_eq!(r_diags[0].line, 3);
        assert_eq!(r_diags[0].col, 8);
        assert_eq!(r_diags[0].code.as_deref(), Some("F401"));
        assert_eq!(r_diags[0].help.as_deref(), Some("Remove unused import: `os`"));
    }

    #[test]
    fn scope_for_narrows_cargo_and_tsc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/foo\", \"crates/bar\"]\n").unwrap();
        let foo = root.join("crates").join("foo");
        std::fs::create_dir_all(foo.join("src")).unwrap();
        std::fs::write(foo.join("Cargo.toml"), "[package]\nname = \"foo-crate\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(foo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();

        clear_detect_cache();
        let spec = detect(root).expect("detect cargo");
        assert_eq!(spec.kind, CheckKind::Cargo);
        let narrowed = spec.scope_for(&[PathBuf::from("crates/foo/src/lib.rs")]);
        assert!(
            narrowed.argv.iter().any(|a| a == "--workspace") && narrowed.argv.iter().any(|a| a == "--all-targets") && !narrowed.argv.iter().any(|a| a == "-p"),
            "an edit is checked against its dependents and tests: {:?}",
            narrowed.argv
        );

        // TSC project narrowing
        let sub = root.join("packages").join("web");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        std::fs::write(sub.join("tsconfig.json"), "{}").unwrap();
        let tsc_spec = CheckSpec {
            kind: CheckKind::Tsc,
            argv: vec!["tsc".into(), "--noEmit".into(), "-p".into(), "tsconfig.json".into()],
            cwd: root.to_path_buf(),
        };
        let tsc_narrowed = tsc_spec.scope_for(&[PathBuf::from("packages/web/src/app.ts")]);
        assert!(
            tsc_narrowed.argv.windows(2).any(|w| w == ["-p", "packages/web/tsconfig.json"]),
            "expected narrowed tsconfig in {:?}",
            tsc_narrowed.argv
        );

        // Cross-repo editing: when session cwd is in `aporia` (its own Cargo workspace), editing a
        // file in sibling repo `figura` selects `figura`'s Cargo workspace, not `aporia`'s.
        let aporia = tmp.path().join("aporia");
        let figura = tmp.path().join("figura");
        std::fs::create_dir_all(aporia.join(".git")).unwrap();
        std::fs::create_dir_all(aporia.join("src")).unwrap();
        std::fs::write(aporia.join("Cargo.toml"), "[package]\nname = \"aporia\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::create_dir_all(figura.join(".git")).unwrap();
        std::fs::create_dir_all(figura.join("crates/core/src")).unwrap();
        std::fs::write(figura.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/core\"]\n").unwrap();
        std::fs::write(figura.join("crates/core/Cargo.toml"), "[package]\nname = \"figura-core\"\nversion = \"0.1.0\"\n").unwrap();
        let edited = figura.join("crates/core/src/lib.rs");
        std::fs::write(&edited, "pub fn draw() {}\n").unwrap();
        let cross = detect_for_files(&aporia, &[edited]).expect("detect figura cargo");
        assert_eq!(
            crate::paths::canonicalize(&cross.cwd).unwrap(),
            crate::paths::canonicalize(&figura).unwrap(),
            "edited file's workspace wins over session cwd's workspace"
        );
    }

    #[test]
    fn impact_extracts_changed_defs_and_merges_with_diagnostics() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,5 +1,5 @@
-pub fn compute_total(x: i32) -> i32 {
+pub fn compute_total(x: i32, y: i32) -> i32 {
     x
 }
-pub struct OldWidget {
+pub struct NewWidget {
";
        let syms = extract_changed_definitions(diff);
        assert_eq!(syms, vec!["compute_total", "OldWidget", "NewWidget"]);

        let mut raw_refs = Vec::new();
        // Same line as a diagnostic -> diagnostic wins
        raw_refs.push(ImpactRef {
            symbol: "compute_total".into(),
            file: "src/caller.rs".into(),
            line: 10,
            text: "let v = compute_total(1);".into(),
        });
        // Edited file -> excluded
        raw_refs.push(ImpactRef {
            symbol: "compute_total".into(),
            file: "src/lib.rs".into(),
            line: 20,
            text: "compute_total(1, 2)".into(),
        });
        // Ignored dir -> excluded
        raw_refs.push(ImpactRef {
            symbol: "compute_total".into(),
            file: "target/debug/build/out.rs".into(),
            line: 5,
            text: "compute_total(1)".into(),
        });
        // 25 unedited caller lines -> capped at 20
        for i in 1..=25 {
            raw_refs.push(ImpactRef {
                symbol: "compute_total".into(),
                file: format!("src/other_{i}.rs"),
                line: i,
                text: format!("let _ = compute_total({i});"),
            });
        }
        let diags = vec![Diagnostic {
            file: "src/caller.rs".into(),
            line: 10,
            col: 9,
            severity: "error".into(),
            code: Some("E0061".into()),
            message: "this function takes 2 arguments".into(),
            help: None,
        }];
        let merged = merge_impact_refs(
            raw_refs,
            &[PathBuf::from("src/lib.rs")],
            &diags,
            Path::new("/repo"),
            IMPACT_MAX_LINES,
        );
        assert_eq!(merged.len(), IMPACT_MAX_LINES);
        assert!(!merged.iter().any(|r| r.file == "src/caller.rs" && r.line == 10));
        assert!(!merged.iter().any(|r| r.file == "src/lib.rs"));
        assert!(!merged.iter().any(|r| r.file.starts_with("target/")));
        assert_eq!(merged[0].file, "src/other_1.rs");
    }

    /// A file edited through a symlinked path (macOS `/tmp` → `/private/tmp`, a symlinked
    /// home) is still the edited file: its own lines are never reported as callers.
    #[cfg(unix)]
    #[test]
    fn impact_excludes_edited_file_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::fs::write(real.join("src/lib.rs"), "pub fn reconcile() {}\n").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let refs = |file: &str| ImpactRef { symbol: "reconcile".into(), file: file.into(), line: 1, text: "reconcile()".into() };
        let merged = merge_impact_refs(
            vec![refs("src/lib.rs"), refs("src/api.rs")],
            &[link.join("src/lib.rs")],
            &[],
            &std::fs::canonicalize(&real).unwrap(),
            IMPACT_MAX_LINES,
        );
        assert_eq!(merged.iter().map(|r| r.file.as_str()).collect::<Vec<_>>(), ["src/api.rs"]);
    }
}
