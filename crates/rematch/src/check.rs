//! Fast compiler/type/lint check execution, git diff/grep impact collection, and
//! `rematch check` CLI command (DESIGN §10, §16).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;
use rematch_core::check::{
    self, CheckKind, CheckReport, CheckSpec, Diagnostic, ImpactRef, DEFAULT_CHECK_BUDGET_BYTES,
    DEFAULT_CHECK_TIMEOUT_SECS, IMPACT_MAX_LINES,
};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::proc::run_bounded;

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    /// Changed files to scope the check and impact analysis to.
    #[arg(long = "files", value_name = "FILE", num_args = 0..)]
    pub files: Vec<PathBuf>,
    /// Detect the repo's check spec without running it.
    #[arg(long)]
    pub detect_only: bool,
    /// Timeout in seconds (default 30).
    #[arg(long, default_value_t = DEFAULT_CHECK_TIMEOUT_SECS)]
    pub timeout: u32,
    /// Byte budget for the rendered context message (default 4096).
    #[arg(long, default_value_t = DEFAULT_CHECK_BUDGET_BYTES)]
    pub budget_bytes: u32,
    /// Optional hook id and session id to record a `check_ran` trace event when run by a host.
    #[arg(long, hide = true)]
    pub hook_id: Option<String>,
    #[arg(long, hide = true)]
    pub session: Option<String>,
}

/// Run `spec` scoped to `files` with bounded `timeout`, returning a [`CheckReport`]
/// (diagnostics parsed, without git impact collection).
pub fn run(spec: &CheckSpec, files: &[PathBuf], timeout: Duration) -> CheckReport {
    let scoped = spec.scope_for(files);
    let started = Instant::now();
    let Some((prog, args)) = scoped.argv.split_first() else {
        return CheckReport {
            ok: true,
            kind: Some(scoped.kind),
            command: scoped.argv,
            duration_ms: 0,
            ..Default::default()
        };
    };

    let cmd = build_command(prog, args, &scoped.cwd);
    let res = match run_bounded(cmd, timeout) {
        Ok(b) => b,
        Err(e) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut rep = CheckReport {
                ok: false,
                kind: Some(scoped.kind),
                command: scoped.argv,
                duration_ms,
                timed_out: false,
                diagnostics: vec![Diagnostic {
                    file: String::new(),
                    line: 1,
                    col: 1,
                    severity: "error".into(),
                    code: None,
                    message: format!("failed to spawn check: {e}"),
                    help: None,
                }],
                impact: Vec::new(),
                truncated: false,
                rendered: None,
            };
            let (rendered, truncated) = check::render_report(&rep, DEFAULT_CHECK_BUDGET_BYTES as usize);
            rep.truncated = truncated;
            rep.rendered = Some(rendered);
            return rep;
        }
    };

    let mut duration_ms = started.elapsed().as_millis() as u64;
    let mut timed_out = res.killed;
    let mut exit_ok = !timed_out && res.code == Some(0);
    let stdout = &res.stdout;
    let stderr = &res.stderr;
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };

    let mut diagnostics = match scoped.kind {
        CheckKind::Cargo => {
            let mut diags = check::parse_cargo_json(stdout, &scoped.cwd);
            if diags.is_empty() && !exit_ok && !timed_out {
                let msg = stderr
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("cargo check failed");
                diags.push(Diagnostic {
                    file: "Cargo.toml".into(),
                    line: 1,
                    col: 1,
                    severity: "error".into(),
                    code: None,
                    message: msg.to_string(),
                    help: None,
                });
            }
            diags
        }
        CheckKind::Tsc => check::parse_tsc_output(&combined, &scoped.cwd),
        CheckKind::Ruff => check::parse_ruff_json(stdout, &scoped.cwd),
        CheckKind::Pyright => check::parse_pyright_json(stdout, &scoped.cwd),
        CheckKind::Mypy => check::parse_mypy_output(&combined, &scoped.cwd),
        CheckKind::Go => check::parse_go_output(&combined, &scoped.cwd),
    };

    // For Go: `go vet` also runs a type/build pass, but if `go vet` passed and we have time left,
    // also verify `go build` on the same package args when `argv` started with `go vet`.
    if scoped.kind == CheckKind::Go && exit_ok && scoped.argv.get(1).map(String::as_str) == Some("vet") {
        let elapsed = started.elapsed();
        if let Some(rem) = timeout.checked_sub(elapsed).filter(|d| !d.is_zero()) {
            let pkg_args: Vec<String> = scoped.argv.iter().skip(2).cloned().collect();
            let mut build_args = vec!["build".to_string()];
            build_args.extend(pkg_args);
            let b_cmd = build_command(prog, &build_args, &scoped.cwd);
            if let Ok(b_res) = run_bounded(b_cmd, rem) {
                duration_ms = started.elapsed().as_millis() as u64;
                if b_res.killed {
                    timed_out = true;
                    exit_ok = false;
                } else if b_res.code != Some(0) {
                    exit_ok = false;
                    let b_out = format!("{}\n{}", b_res.stdout, b_res.stderr);
                    diagnostics.extend(check::parse_go_output(&b_out, &scoped.cwd));
                }
            }
        }
    }

    let has_errors = diagnostics.iter().any(|d| d.severity == "error");
    let ok = exit_ok && !has_errors && !timed_out;

    let mut rep = CheckReport {
        ok,
        kind: Some(scoped.kind),
        command: scoped.argv,
        duration_ms,
        timed_out,
        diagnostics,
        impact: Vec::new(),
        truncated: false,
        rendered: None,
    };
    let (rendered, truncated) = check::render_report(&rep, DEFAULT_CHECK_BUDGET_BYTES as usize);
    rep.truncated = truncated;
    rep.rendered = Some(rendered);
    rep
}

/// Run `spec` scoped to `files`, collect `git diff` + `git grep` impact references in unedited
/// files, merge with diagnostics, and render within `budget_bytes`.
pub fn run_with_impact(
    spec: &CheckSpec,
    files: &[PathBuf],
    timeout: Duration,
    budget_bytes: usize,
) -> CheckReport {
    let deadline = Instant::now() + timeout;
    let mut rep = run(spec, files, timeout);
    if !files.is_empty() && !rep.timed_out && Instant::now() < deadline {
        let raw_impact = collect_impact(&spec.cwd, files, deadline);
        rep.impact = check::merge_impact_refs(
            raw_impact,
            files,
            &rep.diagnostics,
            &spec.cwd,
            IMPACT_MAX_LINES,
        );
    }
    let (rendered, truncated) = check::render_report(&rep, budget_bytes);
    rep.truncated = truncated;
    rep.rendered = Some(rendered);
    rep
}

fn build_command(prog: &str, args: &[String], cwd: &Path) -> Command {
    #[cfg(windows)]
    let mut cmd = if prog.to_ascii_lowercase().ends_with(".cmd") || prog.to_ascii_lowercase().ends_with(".bat") {
        let mut c = Command::new("cmd.exe");
        c.args(["/d", "/s", "/c", prog]);
        c.args(args);
        c
    } else {
        let mut c = Command::new(prog);
        c.args(args);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new(prog);
        c.args(args);
        c
    };
    cmd.current_dir(cwd);
    cmd
}

/// Run `git diff HEAD -- <files>` (falling back to `git diff -- <files>`) to find changed
/// definitions, then `git grep -n -w` for those symbols across the repo.
pub fn collect_impact(repo_top: &Path, files: &[PathBuf], deadline: Instant) -> Vec<ImpactRef> {
    if files.is_empty() || !repo_top.join(".git").exists() {
        return Vec::new();
    }
    let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
        return Vec::new();
    };
    let rel_files: Vec<String> = files
        .iter()
        .map(|f| {
            let p = if f.is_absolute() {
                f.strip_prefix(repo_top).unwrap_or(f)
            } else {
                f.as_path()
            };
            p.to_string_lossy().replace('\\', "/")
        })
        .collect();

    let diff_budget = Duration::from_secs(3).min(rem);
    let mut diff_cmd = Command::new("git");
    diff_cmd
        .current_dir(repo_top)
        .args(["diff", "-U0", "HEAD", "--"])
        .args(&rel_files);
    let diff_out = match run_bounded(diff_cmd, diff_budget) {
        Ok(b) if b.code == Some(0) && !b.stdout.is_empty() => b.stdout,
        _ => {
            let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return Vec::new();
            };
            let mut fallback = Command::new("git");
            fallback
                .current_dir(repo_top)
                .args(["diff", "-U0", "--"])
                .args(&rel_files);
            match run_bounded(fallback, Duration::from_secs(3).min(rem)) {
                Ok(b) if !b.stdout.is_empty() => b.stdout,
                _ => return Vec::new(),
            }
        }
    };

    let symbols = check::extract_changed_definitions(&diff_out);
    if symbols.is_empty() {
        return Vec::new();
    }

    let mut raw_refs = Vec::new();
    for sym in symbols.into_iter().take(6) {
        let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
            break;
        };
        let mut grep_cmd = Command::new("git");
        grep_cmd
            .current_dir(repo_top)
            .args([
                "grep",
                "-n",
                "-w",
                "-I",
                "--no-color",
                &sym,
                "--",
                ".",
                ":(exclude)target",
                ":(exclude)node_modules",
                ":(exclude)vendor",
                ":(exclude)dist",
            ]);
        let Ok(b) = run_bounded(grep_cmd, Duration::from_secs(2).min(rem)) else { continue };
        let text = &b.stdout;
        for line in text.lines().take(60) {
            // Format: file:line:text
            let mut parts = line.splitn(3, ':');
            let (Some(file), Some(line_str), Some(snippet)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let Ok(line_num) = line_str.parse::<u32>() else { continue };
            raw_refs.push(ImpactRef {
                symbol: sym.clone(),
                file: file.replace('\\', "/"),
                line: line_num,
                text: snippet.trim().to_string(),
            });
        }
    }
    raw_refs
}

/// CLI handler for `rematch check`.
pub fn cmd_check(ctx: &AppCtx, args: CheckArgs) -> Result<Value> {
    let repo_top = check::find_repo_top(&ctx.cwd);
    let resolved_files: Vec<PathBuf> = args
        .files
        .iter()
        .map(|f| if f.is_absolute() { f.clone() } else { ctx.cwd.join(f) })
        .collect();
    let spec = check::detect_for_files(&repo_top, &resolved_files);

    if args.detect_only {
        let scoped = spec.as_ref().map(|s| s.scope_for(&resolved_files));
        let lines = match &scoped {
            Some(s) => vec![format!(
                "detected {} in {}: {}",
                s.kind.as_str(),
                s.cwd.display(),
                s.argv.join(" ")
            )],
            None => vec![format!("no fast check detected for {}", repo_top.display())],
        };
        return Ok(json!({
            "detected": scoped.is_some(),
            "spec": scoped,
            "lines": lines,
        }));
    }

    let normalized_files = normalize_check_files(&repo_top, &resolved_files);
    let timeout = Duration::from_secs(u64::from(args.timeout.max(1)));
    let Some(spec) = spec else {
        let mut lines = Vec::new();
        // Even without a compiler check, if files were edited in a git repo we can still surface impact refs.
        let impact = if !resolved_files.is_empty() {
            let raw = collect_impact(&repo_top, &resolved_files, Instant::now() + timeout);
            check::merge_impact_refs(raw, &resolved_files, &[], &repo_top, IMPACT_MAX_LINES)
        } else {
            Vec::new()
        };
        let rendered = if !impact.is_empty() {
            let rep = CheckReport {
                ok: true,
                kind: None,
                command: Vec::new(),
                duration_ms: 0,
                timed_out: false,
                diagnostics: Vec::new(),
                impact: impact.clone(),
                truncated: false,
                rendered: None,
            };
            let (r, _) = check::render_report(&rep, args.budget_bytes as usize);
            lines.push(r.clone());
            Some(r)
        } else {
            lines.push("no fast check detected".to_string());
            None
        };
        return Ok(json!({
            "detected": false,
            "ok": true,
            "kind": Value::Null,
            "command": Vec::<String>::new(),
            "duration_ms": 0,
            "timed_out": false,
            "diagnostics": Vec::<Diagnostic>::new(),
            "impact": impact,
            "truncated": false,
            "rendered": rendered,
            "files": normalized_files,
            "lines": lines,
        }));
    };

    let report = run_with_impact(&spec, &resolved_files, timeout, args.budget_bytes as usize);
    let errors = report.diagnostics.iter().filter(|d| d.severity == "error").count() as u32;
    let rendered = report.rendered.clone().unwrap_or_default();

    // If a session and hook_id were supplied, append `check_ran` to the trace file.
    if let (Some(sid), Some(hook_id)) = (args.session.as_deref(), args.hook_id.as_deref()) {
        if !sid.is_empty() && !hook_id.is_empty() {
            let recorded = crate::trace_event::TraceFile::open(ctx, sid)
                .and_then(|mut tf| push_check_ran(&mut tf, hook_id, &spec, &report, &resolved_files));
            if let Err(e) = recorded {
                eprintln!("rematch: check_ran not recorded for session {sid}: {e:#}");
            }
        }
    }

    let lines = if rendered.is_empty() {
        Vec::new()
    } else {
        rendered.lines().map(str::to_string).collect()
    };

    Ok(json!({
        "detected": true,
        "ok": report.ok,
        "kind": report.kind,
        "command": report.command,
        "duration_ms": report.duration_ms,
        "timed_out": report.timed_out,
        "errors": errors,
        "diagnostics": report.diagnostics,
        "impact": report.impact,
        "truncated": report.truncated,
        "rendered": report.rendered,
        "files": normalized_files,
        "lines": lines,
    }))
}

/// Normalize `files` relative to `repo_top` with forward slashes and no `./` prefix.
pub fn normalize_check_files(repo_top: &Path, files: &[PathBuf]) -> Vec<String> {
    let canon_top = rematch_core::paths::canonicalize(repo_top).unwrap_or_else(|_| repo_top.to_path_buf());
    let mut out = Vec::new();
    for f in files {
        let canon_f = rematch_core::paths::canonicalize(f).unwrap_or_else(|_| f.clone());
        let rel = canon_f
            .strip_prefix(&canon_top)
            .or_else(|_| f.strip_prefix(repo_top))
            .unwrap_or(f.as_path());
        let s = rel.to_string_lossy().replace('\\', "/");
        let s = s.trim_start_matches("./").to_string();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Append `check_ran` for `report` (one run of `spec` over `files`) to the session trace.
pub(crate) fn push_check_ran(
    tf: &mut crate::trace_event::TraceFile,
    hook_id: &str,
    spec: &CheckSpec,
    report: &CheckReport,
    files: &[PathBuf],
) -> Result<()> {
    let repo_top = check::find_repo_top(&spec.cwd);
    let seq = tf.seq();
    tf.push(rematch_core::trace::TraceRecord::CheckRan {
        seq,
        t: tf.now,
        hook_id: hook_id.to_string(),
        kind: spec.kind.as_str().to_string(),
        ok: report.ok,
        errors: report.diagnostics.iter().filter(|d| d.severity == "error").count() as u32,
        duration_ms: report.duration_ms,
        injected_bytes: report.rendered.as_deref().map_or(0, str::len) as u64,
        files: normalize_check_files(&repo_top, files),
    })
}
