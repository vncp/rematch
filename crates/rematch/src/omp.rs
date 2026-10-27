//! omp invocation builders and JSON event-stream verification (DESIGN §9, §14; the forge
//! `omp_runner` contract): separate argv, provider+model verified on every assistant
//! message, process-group cancellation, bounded time.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rematch_core::paths::RematchPaths;
use rematch_core::types::ModelRef;
use serde_json::Value;

use crate::proc::run_bounded_input;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// `$OMP_BIN` → `omp` on PATH.
pub fn omp_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("OMP_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    which("omp").unwrap_or_else(|| PathBuf::from("omp"))
}

/// First executable `name` on `$PATH` (Windows: `name` + each `%PATHEXT%` extension).
pub fn which(name: &str) -> Option<PathBuf> {
    which_in(&std::env::var_os("PATH")?, name)
}

/// [`which`] over the search path `path`.
fn which_in(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|d| candidates(&d, name).into_iter().find(|p| is_executable(p)))
}

#[cfg(unix)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).map(|e| dir.join(format!("{name}{}", e.to_lowercase()))).collect()
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

pub fn selector(m: &ModelRef) -> String {
    format!("{}/{}", m.provider, m.id)
}

#[derive(Debug, Clone)]
pub struct NoToolsCall {
    pub model: ModelRef,
    pub system_prompt_file: PathBuf,
    pub prompt: String,
    pub timeout: Duration,
}

/// §9 argv, exactly (program not included).
pub fn no_tools_argv(call: &NoToolsCall, cwd: &Path, no_tools_config: &Path) -> Vec<String> {
    let s = |x: &str| x.to_string();
    vec![
        s("--cwd"),
        cwd.to_string_lossy().into_owned(),
        s("--model"),
        selector(&call.model),
        s("--no-tools"),
        s("--system-prompt"),
        call.system_prompt_file.to_string_lossy().into_owned(),
        s("--config"),
        no_tools_config.to_string_lossy().into_owned(),
        s("--no-prewalk"),
        s("--no-session"),
        s("--no-title"),
        s("--no-extensions"),
        s("--no-skills"),
        s("--no-rules"),
        s("--no-lsp"),
        s("--mode"),
        s("json"),
        s("-p"),
        s("--"),
        call.prompt.clone(),
    ]
}

#[derive(Debug, Clone)]
pub struct ToolsCall {
    pub model: ModelRef,
    pub cwd: PathBuf,
    pub prompt: String,
    pub tools: Option<String>,
    pub max_time: Duration,
    pub config_overlay: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

/// `--cwd --model --no-session --no-title --no-extensions --no-rules --no-lsp
/// --approval-mode yolo --mode json --max-time -p -- prompt` (+ `--no-skills` unless an
/// overlay supplies customDirectories, + `--config`, + `--tools`).
pub fn tools_argv(call: &ToolsCall) -> Vec<String> {
    let s = |x: &str| x.to_string();
    let mut v = vec![
        s("--cwd"),
        call.cwd.to_string_lossy().into_owned(),
        s("--model"),
        selector(&call.model),
        s("--no-session"),
        s("--no-title"),
        s("--no-extensions"),
        s("--no-rules"),
        s("--no-lsp"),
        s("--approval-mode"),
        s("yolo"),
    ];
    match &call.config_overlay {
        Some(c) => {
            v.push(s("--config"));
            v.push(c.to_string_lossy().into_owned());
        }
        None => v.push(s("--no-skills")),
    }
    if let Some(t) = &call.tools {
        v.push(s("--tools"));
        v.push(t.clone());
    }
    v.extend(call.extra_args.iter().cloned());
    v.extend([
        s("--mode"),
        s("json"),
        s("--max-time"),
        format!("{}s", call.max_time.as_secs().max(1)),
        s("-p"),
        s("--"),
        call.prompt.clone(),
    ]);
    v
}

/// PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0
pub fn child_env() -> Vec<(String, String)> {
    vec![
        ("PI_OFFLINE".into(), "1".into()),
        ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
        ("PI_TELEMETRY".into(), "0".into()),
    ]
}

/// Every non-empty line must be a JSON value.
pub fn parse_events(stdout: &str) -> Result<Vec<Value>> {
    stdout
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l.trim()).with_context(|| format!("omp event line {} is not JSON", i + 1)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct Verified {
    pub text: String,
    pub events: Vec<Value>,
    pub usage: Option<Value>,
}

fn assistant_messages(events: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    for ev in events {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "message_start" | "message_update" | "message_end" | "assistant_message" => {
                if let Some(m) = ev.get("message").filter(|m| is_assistant(m)) {
                    out.push(m);
                }
            }
            "agent_end" => {
                if let Some(ms) = ev.get("messages").and_then(Value::as_array) {
                    out.extend(ms.iter().filter(|m| is_assistant(m)));
                }
            }
            _ => {}
        }
    }
    out
}

fn is_assistant(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("assistant")
}

fn text_of(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// §9 rules: every assistant message carries the expected provider+model (and matching
/// upstream* when present); any `errorMessage`, `stopReason ∈ {error, aborted}`, or (with
/// `no_tools`) a tool-call block fails the run. The reply is the last completed message.
pub fn verify_stream(events: Vec<Value>, expected: &ModelRef, no_tools: bool) -> Result<Verified> {
    let msgs = assistant_messages(&events);
    if msgs.is_empty() {
        bail!("omp emitted no assistant message events");
    }
    for m in &msgs {
        let provider = m.get("provider").and_then(Value::as_str);
        let model = m.get("model").and_then(Value::as_str);
        if provider != Some(expected.provider.as_str()) || model != Some(expected.id.as_str()) {
            bail!(
                "assistant message from {}/{} but {} was requested (no fallback)",
                provider.unwrap_or("?"),
                model.unwrap_or("?"),
                selector(expected)
            );
        }
        if let Some(up) = m.get("upstreamProvider").and_then(Value::as_str) {
            if up != expected.provider {
                bail!("assistant upstream provider {up} != {}", expected.provider);
            }
        }
        if let Some(up) = m.get("upstreamModel").and_then(Value::as_str) {
            if up != expected.id && up != selector(expected) {
                bail!("assistant upstream model {up} != {}", expected.id);
            }
        }
        if let Some(err) = m.get("errorMessage").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            bail!("assistant reported error: {err}");
        }
        if let Some(sr @ ("error" | "aborted")) = m.get("stopReason").and_then(Value::as_str) {
            bail!("assistant stopReason {sr}");
        }
        if no_tools {
            if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                if blocks.iter().any(|b| {
                    matches!(b.get("type").and_then(Value::as_str), Some("toolCall" | "tool_use" | "functionCall"))
                }) {
                    bail!("assistant emitted a tool call under --no-tools");
                }
            }
        }
    }
    let completed: Vec<&Value> = events
        .iter()
        .filter(|ev| matches!(ev.get("type").and_then(Value::as_str), Some("message_end" | "assistant_message")))
        .filter_map(|ev| ev.get("message").filter(|m| is_assistant(m)))
        .collect();
    let last = completed
        .last()
        .copied()
        .or_else(|| msgs.last().copied())
        .ok_or_else(|| anyhow!("omp emitted no completed assistant response"))?;
    if let Some(sr) = last.get("stopReason").and_then(Value::as_str) {
        if !matches!(sr, "stop" | "end_turn") {
            bail!("assistant response did not complete normally (stopReason {sr})");
        }
    }
    let text = text_of(last);
    if text.trim().is_empty() {
        bail!("assistant response has no text");
    }
    let usage = last.get("usage").cloned();
    Ok(Verified { text, events, usage })
}

/// An omp run: `bin argv…` with [`child_env`], and the bytes for its stdin. A `.cmd`/`.bat`
/// launcher (npm's Windows shim) runs through cmd.exe, which ends a command line at a newline
/// and caps it at 8191 characters, so it never gets the prompt as an argument: the trailing
/// `-- <prompt>` is dropped and the prompt is piped on stdin, which omp takes verbatim as the
/// prompt (never as a command). Any other program gets the argv unchanged.
pub fn command(bin: &Path, argv: &[String]) -> (Command, Option<Vec<u8>>) {
    let (args, input) = match argv {
        [head @ .., sep, prompt] if sep == "--" && is_batch(bin) => (head, Some(prompt.clone().into_bytes())),
        _ => (argv, None),
    };
    let mut c = Command::new(bin);
    c.args(args).envs(child_env());
    (c, input)
}

/// A Windows batch script (`.cmd`/`.bat`): Windows runs it through cmd.exe.
fn is_batch(bin: &Path) -> bool {
    cfg!(windows) && bin.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

fn check_exit(b: &crate::proc::Bounded, what: &str) -> Result<()> {
    if b.killed {
        bail!("{what} timed out after {} ms", b.elapsed_ms);
    }
    if b.code != Some(0) {
        bail!("{what} exited with {:?}: {}", b.code, b.stderr.trim().chars().take(600).collect::<String>());
    }
    Ok(())
}

/// Temporary cwd, bounded run, verification.
pub fn run_no_tools(call: &NoToolsCall, paths: &RematchPaths) -> Result<Verified> {
    let tmp = tempfile::tempdir().context("tmp cwd for omp")?;
    let argv = no_tools_argv(call, tmp.path(), &paths.improver.join("omp-no-tools.yml"));
    let (mut c, input) = command(&omp_bin(), &argv);
    c.current_dir(tmp.path());
    let b = run_bounded_input(c, call.timeout, input.as_deref())?;
    check_exit(&b, "omp -p --no-tools")?;
    verify_stream(parse_events(&b.stdout)?, &call.model, true)
}

pub fn run_tools(call: &ToolsCall) -> Result<Verified> {
    let argv = tools_argv(call);
    let (mut c, input) = command(&omp_bin(), &argv);
    c.current_dir(&call.cwd);
    // max_time is enforced by omp; the outer bound adds a grace for teardown.
    let b = run_bounded_input(c, call.max_time + Duration::from_secs(30), input.as_deref())?;
    check_exit(&b, "omp -p")?;
    verify_stream(parse_events(&b.stdout)?, &call.model, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m() -> ModelRef {
        ModelRef { provider: "anthropic".into(), id: "claude-opus-5-5".into() }
    }

    fn msg(provider: &str, model: &str, text: &str) -> Value {
        json!({"role":"assistant","provider":provider,"model":model,"content":[{"type":"text","text":text}],"stopReason":"stop"})
    }

    #[test]
    fn argv_is_exact() {
        let call = NoToolsCall { model: m(), system_prompt_file: "/i/editor.md".into(), prompt: "P".into(), timeout: DEFAULT_TIMEOUT };
        let v = no_tools_argv(&call, Path::new("/tmp/x"), Path::new("/i/omp-no-tools.yml"));
        assert_eq!(
            v.join(" "),
            "--cwd /tmp/x --model anthropic/claude-opus-5-5 --no-tools --system-prompt /i/editor.md --config /i/omp-no-tools.yml --no-prewalk --no-session --no-title --no-extensions --no-skills --no-rules --no-lsp --mode json -p -- P"
        );
    }

    #[test]
    fn an_executable_gets_the_prompt_in_argv() {
        let argv: Vec<String> = ["--mode", "json", "-p", "--", "line one\nline two"].map(String::from).into();
        let (c, input) = command(Path::new("omp"), &argv);
        assert_eq!(c.get_args().collect::<Vec<_>>(), argv.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>());
        assert_eq!(input, None);
    }

    /// Set to a directory, [`omp_fixture`] acts as omp behind a `.cmd` launcher: it records
    /// its stdin there and writes the event stream the launcher prints.
    #[cfg(windows)]
    const FIXTURE_DIR: &str = "REMATCH_OMP_FIXTURE_DIR";

    #[cfg(windows)]
    #[test]
    #[ignore = "child process of batch_launcher_gets_the_prompt_on_stdin"]
    fn omp_fixture() {
        let Some(dir) = std::env::var_os(FIXTURE_DIR).map(std::path::PathBuf::from) else { return };
        let mut stdin = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut stdin).unwrap();
        std::fs::write(dir.join("stdin.bin"), stdin).unwrap();
        let ev = json!({"type": "message_end", "message": msg("anthropic", "claude-opus-5-5", "reply")});
        std::fs::write(dir.join("events.jsonl"), format!("{ev}\n")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn batch_launcher_gets_the_prompt_on_stdin() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("npm prefix");
        std::fs::create_dir(&dir).unwrap();
        let bin = dir.join("omp.cmd");
        let exe = std::env::current_exe().unwrap();
        // npm's shim shape: cmd.exe runs the script, which starts the real program with `%*`.
        std::fs::write(
            &bin,
            format!(
                "@echo off\r\n>\"%~dp0args.txt\" echo(%*\r\n\"{}\" omp::tests::omp_fixture --exact --ignored --test-threads=1 >NUL\r\ntype \"%~dp0events.jsonl\"\r\n",
                exe.display()
            ),
        )
        .unwrap();
        let prompt = format!("line one\n\"quoted\" 100% & | < > ^ !bang! %PATH%\nünïcødé 漢字 🎯\n{}\n", "x".repeat(10_000));
        let call = NoToolsCall { model: m(), system_prompt_file: dir.join("editor.md"), prompt: prompt.clone(), timeout: DEFAULT_TIMEOUT };
        let argv = no_tools_argv(&call, &dir, &dir.join("omp-no-tools.yml"));
        let (mut c, input) = command(&bin, &argv);
        c.env(FIXTURE_DIR, &dir);
        let b = run_bounded_input(c, Duration::from_secs(60), input.as_deref()).unwrap();
        check_exit(&b, "omp.cmd").unwrap();
        assert_eq!(verify_stream(parse_events(&b.stdout).unwrap(), &m(), true).unwrap().text, "reply");
        assert_eq!(std::fs::read(dir.join("stdin.bin")).unwrap(), prompt.as_bytes(), "the prompt arrives byte for byte");
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert!(args.trim_end().ends_with("--mode json -p"), "no `-- <prompt>` in the command line: {args}");
        assert!(args.starts_with("--cwd ") && args.len() < 2_000, "{args}");
    }

    #[test]
    fn verifies_provider_on_every_message() {
        let ok = vec![
            json!({"type":"message_start","message":msg("anthropic","claude-opus-5-5","")}),
            json!({"type":"message_end","message":msg("anthropic","claude-opus-5-5","hi")}),
            json!({"type":"agent_end","messages":[msg("anthropic","claude-opus-5-5","hi")]}),
        ];
        assert_eq!(verify_stream(ok, &m(), true).unwrap().text, "hi");
        let bad = vec![
            json!({"type":"message_end","message":msg("anthropic","claude-opus-5-5","hi")}),
            json!({"type":"agent_end","messages":[msg("openai","gpt","hi")]}),
        ];
        assert!(verify_stream(bad, &m(), true).is_err());
    }

    #[test]
    fn rejects_errors_and_tool_calls() {
        let mut e = msg("anthropic", "claude-opus-5-5", "x");
        e["errorMessage"] = json!("boom");
        assert!(verify_stream(vec![json!({"type":"message_end","message":e})], &m(), true).is_err());
        let t = json!({"role":"assistant","provider":"anthropic","model":"claude-opus-5-5","content":[{"type":"toolCall","id":"1","name":"read"}],"stopReason":"stop"});
        assert!(verify_stream(vec![json!({"type":"message_end","message":t})], &m(), true).is_err());
        let mut a = msg("anthropic", "claude-opus-5-5", "x");
        a["stopReason"] = json!("aborted");
        assert!(verify_stream(vec![json!({"type":"message_end","message":a})], &m(), true).is_err());
        let mut u = msg("anthropic", "claude-opus-5-5", "x");
        u["upstreamProvider"] = json!("other");
        assert!(verify_stream(vec![json!({"type":"message_end","message":u})], &m(), true).is_err());
        assert!(verify_stream(vec![], &m(), true).is_err());
    }

    #[test]
    fn parse_events_requires_json_lines() {
        assert_eq!(parse_events("{\"a\":1}\n\n{\"b\":2}\n").unwrap().len(), 2);
        assert!(parse_events("{\"a\":1}\nnope\n").is_err());
    }

    #[test]
    fn which_finds_only_runnable_files_in_path_order() {
        let t = tempfile::tempdir().unwrap();
        let (first, second) = (t.path().join("first dir"), t.path().join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        // What npm leaves next to its Windows shims, and an unrunnable file on Unix.
        std::fs::write(first.join("omp"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(second.join("omp"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(second.join("omp"), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(which_in(&path, "omp"), Some(second.join("omp")), "the first executable on PATH");
        }
        #[cfg(windows)]
        {
            std::fs::write(second.join("omp.exe"), "").unwrap();
            assert_eq!(which_in(&path, "omp"), Some(second.join("omp.exe")), "an extensionless shim never runs natively");
            std::fs::write(first.join("omp.cmd"), "@echo off\r\n").unwrap();
            assert_eq!(which_in(&path, "omp"), Some(first.join("omp.cmd")), "PATH order first, then PATHEXT");
        }
        assert_eq!(which_in(&path, "absent"), None);
    }
}
