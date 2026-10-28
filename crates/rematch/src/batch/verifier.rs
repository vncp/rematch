//! Deterministic verifiers (SPEC "Batch mode": tests, a check script, a parity check
//! against a known result, or a reconcile receipt; never a model's opinion).
//!
//! Spec strings accepted by `run --verifier` / `task add --verifier`:
//! - `check`: [`CheckVerifier`] (the repo's detected fast check via `rematch_core::check`)
//! - `tests` / `test`: [`TestVerifier`] (cargo / pytest / bun autodetected from files)
//! - `parity:<produced>=<expected>`: [`ParityVerifier`]; `<produced>` is relative to the
//!   worktree, `<expected>` a file path or a 64-hex sha256
//! - `receipt:<path>`: [`ReceiptVerifier`]; JSON file (relative to the worktree) with
//!   `verdict: "pass"|"fail"` and optional `score`
//! - `script:<cmd>` or any other string: [`ScriptVerifier`] (`sh -c <cmd>` in the worktree)

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use rematch_core::check::{self, CheckSpec};
use rematch_core::features::head;
use rematch_core::ledger::sha256_hex;
use rematch_core::Verdict;
use serde_json::Value;

use super::tasks::Task;
use crate::proc;

/// Bytes of verifier output kept in a verdict's detail.
const DETAIL_CHARS: usize = 2048;

pub trait Verifier {
    fn check(&self, task: &Task, worktree: &Path) -> Result<Verdict>;
    /// Short label for receipts.
    fn describe(&self) -> String;
}

/// `sh -c <cmd>` in the worktree; exit 0 = pass. The last stdout line that parses as a
/// JSON object with a numeric `score` supplies the score.
pub struct ScriptVerifier {
    pub cmd: String,
}

/// Runs the project's test suite, detected from the files present.
pub struct TestVerifier;
/// Runs the project's detected fast check (`rematch_core::check::detect`).
pub struct CheckVerifier;

/// sha256 of a produced file against an expected file or digest.
pub struct ParityVerifier {
    pub produced: PathBuf,
    pub expected_file: PathBuf,
}

/// A JSON receipt written by the run: `{"verdict":"pass"|"fail", "score"?: n}`.
pub struct ReceiptVerifier {
    pub path: PathBuf,
}

pub fn parse_spec(spec: &str) -> Result<Box<dyn Verifier>> {
    let s = spec.trim();
    if s.is_empty() {
        bail!("empty verifier");
    }
    if s == "check" {
        return Ok(Box::new(CheckVerifier));
    }
    if s == "tests" || s == "test" {
        return Ok(Box::new(TestVerifier));
    }
    if let Some(rest) = s.strip_prefix("parity:") {
        let (produced, expected) =
            rest.split_once('=').ok_or_else(|| anyhow!("parity verifier needs parity:<produced>=<expected>"))?;
        let (produced, expected) = (produced.trim(), expected.trim());
        if produced.is_empty() || expected.is_empty() {
            bail!("parity verifier needs parity:<produced>=<expected>");
        }
        if Path::new(produced).is_absolute() {
            bail!("parity <produced> must be relative to the worktree");
        }
        // A relative expected file resolves against the task repo at check time (the
        // active checkout, never a rollout's worktree); an absolute one must exist now.
        if !is_sha256(expected) && Path::new(expected).is_absolute() && !Path::new(expected).is_file() {
            bail!("parity <expected> {expected:?} is neither a file nor a sha256");
        }
        return Ok(Box::new(ParityVerifier { produced: produced.into(), expected_file: expected.into() }));
    }
    if let Some(rest) = s.strip_prefix("receipt:") {
        let p = rest.trim();
        if p.is_empty() || Path::new(p).is_absolute() {
            bail!("receipt verifier needs receipt:<path relative to the worktree>");
        }
        return Ok(Box::new(ReceiptVerifier { path: p.into() }));
    }
    let cmd = s.strip_prefix("script:").unwrap_or(s).trim();
    if cmd.is_empty() {
        bail!("empty script verifier");
    }
    Ok(Box::new(ScriptVerifier { cmd: cmd.to_string() }))
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn verdict(pass: bool, score: Option<f64>, detail: String) -> Verdict {
    if pass {
        Verdict::Pass { score, detail }
    } else {
        Verdict::Fail { score, detail }
    }
}

pub fn is_pass(v: &Verdict) -> bool {
    matches!(v, Verdict::Pass { .. })
}

pub fn score_of(v: &Verdict) -> Option<f64> {
    match v {
        Verdict::Pass { score, .. } | Verdict::Fail { score, .. } => *score,
    }
}

pub fn detail_of(v: &Verdict) -> &str {
    match v {
        Verdict::Pass { detail, .. } | Verdict::Fail { detail, .. } => detail,
    }
}

/// Verifier time cap: the task's rollout cap, at least a minute.
fn timeout(task: &Task) -> std::time::Duration {
    task.max_time().max(std::time::Duration::from_secs(60))
}

fn run_in(mut cmd: Command, task: &Task, worktree: &Path) -> Result<proc::Bounded> {
    cmd.current_dir(worktree)
        .env("REMATCH_TASK", &task.id)
        .env("REMATCH_WORKTREE", worktree)
        .env("REMATCH_REPO", &task.repo);
    proc::run_bounded(cmd, timeout(task))
}

fn output_detail(b: &proc::Bounded) -> String {
    let mut s = String::new();
    if b.killed {
        s.push_str("[timed out] ");
    }
    match b.code {
        Some(c) => s.push_str(&format!("exit {c}")),
        None => s.push_str("killed by signal"),
    }
    let tail = |t: &str| {
        let t = t.trim();
        let start = t.char_indices().rev().nth(DETAIL_CHARS / 2).map_or(0, |(i, _)| i);
        t[start..].to_string()
    };
    if !b.stdout.trim().is_empty() {
        s.push_str("\nstdout: ");
        s.push_str(&tail(&b.stdout));
    }
    if !b.stderr.trim().is_empty() {
        s.push_str("\nstderr: ");
        s.push_str(&tail(&b.stderr));
    }
    head(&s, DETAIL_CHARS)
}

fn score_from_stdout(stdout: &str) -> Option<f64> {
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find_map(|v| v.get("score").and_then(Value::as_f64))
}

impl Verifier for ScriptVerifier {
    fn check(&self, task: &Task, worktree: &Path) -> Result<Verdict> {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&self.cmd);
        let b = run_in(c, task, worktree)?;
        let pass = b.code == Some(0) && !b.killed;
        Ok(verdict(pass, score_from_stdout(&b.stdout), output_detail(&b)))
    }

    fn describe(&self) -> String {
        format!("script:{}", self.cmd)
    }
}

/// Default verifier spec `(spec, source)` for a task in `repo` when none was explicitly set:
/// the detected fast check (`check`), else the detected test suite (`tests`).
pub fn default_spec_for(repo: &Path) -> Option<(String, String)> {
    if let Some(c) = check::detect(repo) {
        return Some(("check".into(), format!("detected: check ({})", c.kind.as_str())));
    }
    if TestVerifier::detect(repo).is_some() {
        return Some(("tests".into(), "detected: tests".into()));
    }
    None
}

/// A task's explicit verifier, else [`default_spec_for`] its repository.
pub fn resolve_spec_for_task(task: &Task) -> Option<(String, String)> {
    if let Some(v) = task.verifier.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return Some((v.to_string(), "task".into()));
    }
    default_spec_for(&task.repo)
}

impl CheckVerifier {
    pub fn detect(worktree: &Path) -> Option<CheckSpec> {
        check::detect(worktree)
    }
}

impl Verifier for CheckVerifier {
    fn check(&self, task: &Task, worktree: &Path) -> Result<Verdict> {
        let files: Vec<PathBuf> = task.editable.iter().map(PathBuf::from).collect();
        let spec = check::detect_for_files(worktree, &files)
            .ok_or_else(|| anyhow!("no fast check detected in {} (Cargo.toml, tsconfig.json, ruff/mypy/pyright, go.mod)", worktree.display()))?;
        let rep = crate::check::run(&spec, &files, timeout(task));
        let detail = rep.rendered.clone().unwrap_or_else(|| rep.command.join(" "));
        Ok(verdict(rep.ok, None, head(&format!("{}: {detail}", rep.command.join(" ")), DETAIL_CHARS)))
    }

    fn describe(&self) -> String {
        "check".into()
    }
}

impl TestVerifier {
    /// The test command for a worktree, by marker files.
    pub fn detect(worktree: &Path) -> Option<Vec<&'static str>> {
        let has = |f: &str| worktree.join(f).exists();
        if has("Cargo.toml") {
            return Some(vec!["cargo", "test", "--quiet"]);
        }
        if has("pyproject.toml") || has("pytest.ini") || has("setup.cfg") || has("setup.py") || has("tox.ini") {
            return Some(vec!["python3", "-m", "pytest", "-q"]);
        }
        if has("package.json") && (has("bun.lock") || has("bun.lockb") || has("bunfig.toml")) {
            return Some(vec!["bun", "test"]);
        }
        None
    }
}

impl Verifier for TestVerifier {
    fn check(&self, task: &Task, worktree: &Path) -> Result<Verdict> {
        let argv = Self::detect(worktree)
            .ok_or_else(|| anyhow!("no test runner detected in {} (Cargo.toml, pytest, bun)", worktree.display()))?;
        let mut c = Command::new(argv[0]);
        c.args(&argv[1..]);
        let b = run_in(c, task, worktree)?;
        let pass = b.code == Some(0) && !b.killed;
        Ok(verdict(pass, None, format!("{}: {}", argv.join(" "), output_detail(&b))))
    }

    fn describe(&self) -> String {
        "tests".into()
    }
}

impl Verifier for ParityVerifier {
    fn check(&self, task: &Task, worktree: &Path) -> Result<Verdict> {
        let expected_text = self.expected_file.to_string_lossy();
        let expected = if is_sha256(&expected_text) {
            expected_text.to_ascii_lowercase()
        } else {
            // Relative: the file in the task repo itself (clean at HEAD for a probe), never
            // the caller's cwd or the rollout worktree a candidate could edit.
            let file = if self.expected_file.is_absolute() { self.expected_file.clone() } else { task.repo.join(&self.expected_file) };
            let bytes = std::fs::read(&file).with_context(|| format!("read expected {}", file.display()))?;
            sha256_hex(&bytes)
        };
        let produced = worktree.join(&self.produced);
        let Ok(bytes) = std::fs::read(&produced) else {
            return Ok(verdict(false, None, format!("{} was not produced", self.produced.display())));
        };
        let got = sha256_hex(&bytes);
        let pass = got == expected;
        Ok(verdict(pass, None, format!("{} sha256 {got} (expected {expected})", self.produced.display())))
    }

    fn describe(&self) -> String {
        format!("parity:{}={}", self.produced.display(), self.expected_file.display())
    }
}

impl Verifier for ReceiptVerifier {
    fn check(&self, _task: &Task, worktree: &Path) -> Result<Verdict> {
        let p = worktree.join(&self.path);
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Ok(verdict(false, None, format!("receipt {} missing", self.path.display())));
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Ok(verdict(false, None, format!("receipt {} is not JSON", self.path.display())));
        };
        let word = v.get("verdict").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        let score = v.get("score").and_then(Value::as_f64);
        let detail = format!("receipt {} verdict={word:?}", self.path.display());
        match word.as_str() {
            "pass" => Ok(verdict(true, score, detail)),
            "fail" => Ok(verdict(false, score, detail)),
            _ => Ok(verdict(false, score, format!("{detail} (expected pass|fail)"))),
        }
    }

    fn describe(&self) -> String {
        format!("receipt:{}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(dir: &Path) -> Task {
        Task {
            id: "t".into(),
            repo: dir.to_path_buf(),
            prompt: "p".into(),
            verifier: None,
            max_time_secs: 30,
            tools: None,
            seeds: vec![1],
            created_at: 0,
            editable: Vec::new(),
            auto_probe: None,
        }
    }

    #[test]
    fn batch_script_verifier_exit_and_score() {
        let d = tempfile::tempdir().unwrap();
        let t = task(d.path());
        let v = parse_spec("echo noise; echo '{\"score\": 0.75}'").unwrap();
        let r = v.check(&t, d.path()).unwrap();
        assert!(is_pass(&r));
        assert_eq!(score_of(&r), Some(0.75));
        let v = parse_spec("script:test -f marker").unwrap();
        assert!(!is_pass(&v.check(&t, d.path()).unwrap()));
        std::fs::write(d.path().join("marker"), "").unwrap();
        assert!(is_pass(&v.check(&t, d.path()).unwrap()));
    }

    #[test]
    fn batch_parity_and_receipt_verifiers() {
        let d = tempfile::tempdir().unwrap();
        let t = task(d.path());
        let expected = d.path().join("expected.csv");
        std::fs::write(&expected, "a,b\n").unwrap();
        let v = parse_spec(&format!("parity:out.csv={}", expected.display())).unwrap();
        assert!(!is_pass(&v.check(&t, d.path()).unwrap()), "missing output fails");
        std::fs::write(d.path().join("out.csv"), "a,b\n").unwrap();
        assert!(is_pass(&v.check(&t, d.path()).unwrap()));
        let v = parse_spec(&format!("parity:out.csv={}", sha256_hex(b"other"))).unwrap();
        assert!(!is_pass(&v.check(&t, d.path()).unwrap()));
        // An absolute expected file must exist now (`/no/such/file` is not absolute on Windows).
        assert!(parse_spec(&format!("parity:out.csv={}", d.path().join("no-such-file").display())).is_err());

        let v = parse_spec("receipt:receipt.json").unwrap();
        assert!(!is_pass(&v.check(&t, d.path()).unwrap()));
        std::fs::write(d.path().join("receipt.json"), r#"{"verdict":"PASS","score":3}"#).unwrap();
        let r = v.check(&t, d.path()).unwrap();
        assert!(is_pass(&r));
        assert_eq!(score_of(&r), Some(3.0));
        std::fs::write(d.path().join("receipt.json"), r#"{"verdict":"fail"}"#).unwrap();
        assert!(!is_pass(&v.check(&t, d.path()).unwrap()));
    }

    #[test]
    fn batch_test_runner_and_check_detection() {
        let d = tempfile::tempdir().unwrap();
        check::clear_detect_cache();
        assert!(TestVerifier::detect(d.path()).is_none());
        assert!(default_spec_for(d.path()).is_none());
        std::fs::write(d.path().join("pytest.ini"), "").unwrap();
        assert_eq!(TestVerifier::detect(d.path()).unwrap()[0], "python3");
        assert_eq!(default_spec_for(d.path()), Some(("tests".into(), "detected: tests".into())));
        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
        check::clear_detect_cache();
        assert_eq!(TestVerifier::detect(d.path()).unwrap()[0], "cargo");
        assert_eq!(default_spec_for(d.path()), Some(("check".into(), "detected: check (cargo)".into())));
        assert!(parse_spec("check").is_ok());
        assert!(parse_spec("tests").is_ok());
        assert!(parse_spec("  ").is_err());
    }

    #[test]
    fn batch_check_verifier_detects_subdirectory_crate_via_editable_files() {
        let d = tempfile::tempdir().unwrap();
        check::clear_detect_cache();
        let sub = d.path().join("backend");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        std::fs::write(sub.join("Cargo.toml"), "[package]\nname = \"backend\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(sub.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();
        let mut t = task(d.path());
        t.editable = vec!["backend/src/lib.rs".into()];
        let v = parse_spec("check").unwrap();
        assert!(is_pass(&v.check(&t, d.path()).unwrap()), "clean subdirectory crate passes");
        std::fs::write(sub.join("src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 { \"bad\" }\n").unwrap();
        let fail = v.check(&t, d.path()).unwrap();
        assert!(!is_pass(&fail), "broken subdirectory crate fails");
        assert!(detail_of(&fail).contains("E0308"), "{}", detail_of(&fail));
    }
}
