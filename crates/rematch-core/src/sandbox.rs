//! Sandbox dry-run runner for staged scripts (DESIGN §16 "Scripts"): runs each staged script once
//! in an OS sandbox that denies network access and writes outside an isolated temporary directory
//! (`sandbox-exec` on macOS, `bwrap` on Linux when present; unsupported elsewhere / on Windows).
//!
//! The observed dry-run behavior (clean exit, writes inside the scratch temp dir, or attempted
//! writes outside / network access / non-zero exit) is recorded in a receipt certificate blob and
//! surfaced on the `scripts` review item in `actions`; a zero-argument dry run never approves
//! scripts automatically.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::features::head;
use crate::ledger::{now_ms, sha256_hex, Ledger};
use crate::types::{Activation, Entry, Receipt, Sha256Hex, UnixMs};

/// Maximum wall time for one script dry run.
pub const DRY_RUN_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum stdout/stderr characters kept in the observation record.
pub const OUTPUT_HEAD_CHARS: usize = 512;

/// Observed behavior of one staged script during its sandbox dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptDryRun {
    /// Entry-relative path (`scripts/foo.sh`).
    pub script: String,
    /// sha256 of the script bytes when dry-run.
    pub sha256: Sha256Hex,
    /// `"sandbox-exec"` | `"bwrap"` | `"unsupported"`.
    pub runner: String,
    /// Process exit code (`None` when not run or killed on timeout).
    pub exit_code: Option<i32>,
    /// True when the sandbox ran the script, it exited 0, and no sandbox violation occurred.
    pub clean: bool,
    /// Violation details (blocked write target, blocked network, non-zero exit, or timeout).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<String>,
    /// Files created or modified inside the allowed scratch directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub temp_writes: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout_head: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr_head: String,
    pub at: UnixMs,
}

/// Receipt certificate blob recording the sandbox dry-run observation of an entry's scripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxReport {
    pub what: String,
    pub entry: String,
    pub clean: bool,
    pub scripts: Vec<ScriptDryRun>,
    pub at: UnixMs,
}

pub const SANDBOX_REPORT_WHAT: &str = "script_sandbox_dry_run";

impl ScriptDryRun {
    /// Human-readable one-line summary of what this script did or tried in the dry run.
    pub fn summary_line(&self) -> String {
        if self.runner == "unsupported" {
            return format!("{}: sandbox dry run unavailable on this platform", self.script);
        }
        if self.clean {
            return format!("{}: sandbox dry run clean ({}, exit 0)", self.script, self.runner);
        }
        if !self.violations.is_empty() {
            return format!("{} ({}): {}", self.script, self.runner, self.violations.join("; "));
        }
        format!(
            "{} ({}): exited with {:?}",
            self.script,
            self.runner,
            self.exit_code
        )
    }
}

/// Whether the current platform has a working script sandbox runner (`sandbox-exec` on macOS,
/// `bwrap` on Linux).
pub fn available_runner() -> Option<&'static str> {
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
        return Some("sandbox-exec");
    }
    if cfg!(target_os = "linux") && which_bwrap().is_some() {
        return Some("bwrap");
    }
    None
}

fn which_bwrap() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join("bwrap");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Choose the interpreter command for a script (`sh` by default; `python3`, `bash`, `node` when
/// named in a `#!` shebang or extension).
fn interpreter_for(script: &Path, bytes: &[u8]) -> (&'static str, Vec<&'static str>) {
    let text = String::from_utf8_lossy(bytes);
    if let Some(first) = text.lines().next().and_then(|l| l.strip_prefix("#!")) {
        let shebang = first.trim();
        if shebang.contains("python") {
            return ("python3", vec![]);
        }
        if shebang.contains("node") || shebang.contains("bun") {
            return ("node", vec![]);
        }
        if shebang.contains("bash") {
            return ("bash", vec![]);
        }
    }
    match script.extension().and_then(|e| e.to_str()) {
        Some("py") => ("python3", vec![]),
        Some("js") => ("node", vec![]),
        Some("bash") => ("bash", vec![]),
        _ => ("sh", vec![]),
    }
}

/// Run one script inside the platform sandbox with a fresh scratch directory and a probe directory
/// (`probe_dir`) outside the allowed write root (`HOME` and `OUTSIDE_DIR` point to `probe_dir` so
/// writes outside `$TMPDIR` or cwd hit the sandbox write deny).
pub fn dry_run_script(rel: &str, script_path: &Path) -> ScriptDryRun {
    let bytes = std::fs::read(script_path).unwrap_or_default();
    let sha = sha256_hex(&bytes);
    let at = now_ms();
    let Some(runner) = available_runner() else {
        return ScriptDryRun {
            script: rel.to_string(),
            sha256: sha,
            runner: "unsupported".into(),
            exit_code: None,
            clean: false,
            violations: Vec::new(),
            temp_writes: Vec::new(),
            stdout_head: String::new(),
            stderr_head: String::new(),
            at,
        };
    };

    let Ok(scratch_tmp) = tempfile::tempdir() else {
        return ScriptDryRun {
            script: rel.to_string(),
            sha256: sha,
            runner: runner.into(),
            exit_code: None,
            clean: false,
            violations: vec!["could not create scratch tempdir".into()],
            temp_writes: Vec::new(),
            stdout_head: String::new(),
            stderr_head: String::new(),
            at,
        };
    };
    let Ok(probe_tmp) = tempfile::tempdir() else {
        return ScriptDryRun {
            script: rel.to_string(),
            sha256: sha,
            runner: runner.into(),
            exit_code: None,
            clean: false,
            violations: vec!["could not create probe tempdir".into()],
            temp_writes: Vec::new(),
            stdout_head: String::new(),
            stderr_head: String::new(),
            at,
        };
    };

    let scratch = std::fs::canonicalize(scratch_tmp.path()).unwrap_or_else(|_| scratch_tmp.path().to_path_buf());
    let probe = std::fs::canonicalize(probe_tmp.path()).unwrap_or_else(|_| probe_tmp.path().to_path_buf());
    let script_canon = std::fs::canonicalize(script_path).unwrap_or_else(|_| script_path.to_path_buf());

    let (interp, interp_args) = interpreter_for(&script_canon, &bytes);
    let mut cmd = if runner == "sandbox-exec" {
        let escaped = scratch.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let profile = format!(
            "(version 1) (allow default) (deny network*) (deny file-write*) \
             (allow file-write* (subpath \"{escaped}\") (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\"))"
        );
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(profile).arg(interp);
        for a in interp_args {
            c.arg(a);
        }
        c.arg(&script_canon);
        c
    } else {
        let mut c = Command::new("bwrap");
        c.args(["--unshare-all", "--new-session", "--die-with-parent", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
            .arg("--bind")
            .arg(&scratch)
            .arg(&scratch)
            .arg("--chdir")
            .arg(&scratch)
            .arg("--")
            .arg(interp);
        for a in interp_args {
            c.arg(a);
        }
        c.arg(&script_canon);
        c
    };

    cmd.current_dir(&scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TMPDIR", &scratch)
        .env("TMP", &scratch)
        .env("TEMP", &scratch)
        .env("HOME", &probe);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ScriptDryRun {
                script: rel.to_string(),
                sha256: sha,
                runner: runner.into(),
                exit_code: None,
                clean: false,
                violations: vec![format!("spawn failed: {e}")],
                temp_writes: Vec::new(),
                stdout_head: String::new(),
                stderr_head: String::new(),
                at,
            };
        }
    };

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() >= DRY_RUN_TIMEOUT => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => break,
        }
    }

    let out = child.wait_with_output().ok();
    let exit_code = if timed_out { None } else { out.as_ref().and_then(|o| o.status.code()) };
    let stdout = out.as_ref().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let stderr = out.as_ref().map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();

    let temp_writes = collect_files(&scratch);
    let mut violations = extract_violations(&stderr, &String::from_utf8_lossy(&bytes));
    if timed_out {
        violations.push(format!("timed out after {}s", DRY_RUN_TIMEOUT.as_secs()));
    } else if exit_code != Some(0) && violations.is_empty() {
        let first_err = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("non-zero exit");
        violations.push(format!("exit {}: {}", exit_code.unwrap_or(-1), head(first_err.trim(), 200)));
    }
    let clean = !timed_out && exit_code == Some(0) && violations.is_empty();

    ScriptDryRun {
        script: rel.to_string(),
        sha256: sha,
        runner: runner.into(),
        exit_code,
        clean,
        violations,
        temp_writes,
        stdout_head: head(stdout.trim(), OUTPUT_HEAD_CHARS),
        stderr_head: head(stderr.trim(), OUTPUT_HEAD_CHARS),
        at,
    }
}

fn collect_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

/// Extract human-readable write/network violation lines from stderr (and static hints from the
/// script source when the kernel only reports `Operation not permitted`).
fn extract_violations(stderr: &str, script_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let lower = l.to_ascii_lowercase();
        if lower.contains("operation not permitted")
            || lower.contains("permission denied")
            || lower.contains("read-only file system")
            || lower.contains("network is unreachable")
            || lower.contains("could not resolve host")
            || lower.contains("failed to connect")
        {
            out.push(head(l, 240));
        }
    }
    // Also surface explicit redirection targets in the script when a write was denied.
    if out.iter().any(|v| {
        let l = v.to_ascii_lowercase();
        l.contains("operation not permitted") || l.contains("read-only file system") || l.contains("permission denied")
    }) {
        for line in script_text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some((_, rhs)) = trimmed.split_once('>') {
                let target = rhs.trim_start_matches('>').split_whitespace().next().unwrap_or("");
                let target = target.trim_matches(['"', '\'']);
                if !target.is_empty() && !target.starts_with("/dev/null") && !out.iter().any(|v| v.contains(target)) {
                    out.push(format!("attempted write to {target}"));
                }
            }
        }
    }
    out
}

/// Run a sandbox dry run for all scripts of `e`, record a [`SandboxReport`] receipt in `ledger`,
/// and return the report.
pub fn run_entry_scripts(ledger: &Ledger, e: &Entry) -> Result<Option<SandboxReport>> {
    let scripts_dir = e.path.join("scripts");
    if !scripts_dir.is_dir() {
        return Ok(None);
    }
    let mut files = Vec::new();
    let mut stack = vec![scripts_dir];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
    }
    files.sort();
    if files.is_empty() {
        return Ok(None);
    }
    let mut runs = Vec::with_capacity(files.len());
    for p in files {
        let rel = p.strip_prefix(&e.path).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        runs.push(dry_run_script(&rel, &p));
    }
    let clean = runs.iter().all(|r| r.clean);
    let report = SandboxReport {
        what: SANDBOX_REPORT_WHAT.into(),
        entry: e.id.clone(),
        clean,
        scripts: runs,
        at: now_ms(),
    };
    let cert = ledger.put_blob(serde_json::to_string(&report)?.as_bytes())?;
    ledger.insert_receipt(&Receipt {
        certificate: cert,
        entry: e.id.clone(),
        activation: Activation::Personal,
    })?;
    Ok(Some(report))
}

/// Latest recorded [`SandboxReport`] for `entry_id` whose script hashes still match the files on
/// disk.
pub fn latest_report(ledger: &Ledger, e: &Entry) -> Option<SandboxReport> {
    let receipts = ledger.list_receipts(Some(&e.id)).ok()?;
    for row in receipts.into_iter().rev() {
        let Some(blob) = ledger.get_blob(&row.receipt.certificate).ok().flatten() else { continue };
        let Ok(rep) = serde_json::from_slice::<SandboxReport>(&blob) else { continue };
        if rep.what != SANDBOX_REPORT_WHAT {
            continue;
        }
        let matches_disk = rep.scripts.iter().all(|s| {
            std::fs::read(e.path.join(&s.script))
                .ok()
                .is_some_and(|b| sha256_hex(&b) == s.sha256)
        });
        if matches_disk {
            return Some(rep);
        }
    }
    None
}
