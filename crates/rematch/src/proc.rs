//! Bounded process execution with process-group kill (DESIGN §14). Shared by the
//! editor/classifier channel, batch rollouts and verifiers.

use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::envelope::Cancelled;

/// Bytes kept per stream; the rest is drained and dropped so the child never blocks.
pub const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Grace between SIGTERM and SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default)]
pub struct Bounded {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub killed: bool,
    pub elapsed_ms: u64,
}

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    CANCEL.store(true, Ordering::SeqCst);
}

/// SIGINT/SIGTERM set the cancel flag instead of killing the binary, so a running child
/// group is torn down and the envelope reports `cancelled` (exit 130).
pub fn install_cancel_handler() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

pub fn cancel_requested() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/// Request cancellation (tests, internal aborts).
pub fn request_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Spawns `cmd` in its own process group (`process_group(0)`), stdin null, stdout/stderr
/// piped (bounded buffers), waits ≤ `timeout`; on timeout or cancel fires SIGTERM to the
/// group, waits 2 s, SIGKILL to the group (even if the leader exited).
pub fn run_bounded(cmd: Command, timeout: Duration) -> Result<Bounded> {
    run_bounded_input(cmd, timeout, None)
}

/// [`run_bounded`] with optional bytes written to the child's stdin (scripted editors).
pub fn run_bounded_input(mut cmd: Command, timeout: Duration, input: Option<&[u8]>) -> Result<Bounded> {
    let start = Instant::now();
    // A detached job's id must not leak into tools it runs (they would report into it).
    cmd.env_remove(crate::jobs::JOB_ID_ENV);
    isolate(&mut cmd);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = format!("{:?}", cmd.get_program());
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    let pgid = child.id();

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let readers = [
        child.stdout.take().map(|s| (true, Box::new(s) as Box<dyn Read + Send>)),
        child.stderr.take().map(|s| (false, Box::new(s) as Box<dyn Read + Send>)),
    ];
    let mut open = 0;
    for (is_out, mut stream) in readers.into_iter().flatten() {
        open += 1;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = MAX_STREAM_BYTES.saturating_sub(kept.len());
                        kept.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
            }
            let _ = tx.send((is_out, kept));
        });
    }
    drop(tx);

    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }

    let mut killed = false;
    let mut cancelled = false;
    let mut status = None;
    loop {
        if let Some(s) = child.try_wait()? {
            status = Some(s);
            break;
        }
        if cancel_requested() {
            cancelled = true;
        }
        if cancelled || start.elapsed() >= timeout {
            killed = true;
            kill_group(pgid, &mut child, &mut status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Collect the streams. A group member still holding the pipes after the leader is
    // gone gets SIGTERM, then SIGKILL; after that round the pipes are abandoned (a member
    // that left the group cannot be signalled through it).
    let mut out = Bounded { killed, ..Default::default() };
    let mut deadline = Instant::now() + KILL_GRACE;
    // 0: nothing sent yet (or the timeout path already sent SIGTERM), 1: SIGTERM sent, 2: SIGKILL sent.
    let mut stage = if killed { 1 } else { 0 };
    while open > 0 {
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok((true, b)) => {
                out.stdout = String::from_utf8_lossy(&b).into_owned();
                open -= 1;
            }
            Ok((false, b)) => {
                out.stderr = String::from_utf8_lossy(&b).into_owned();
                open -= 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match stage {
                    // Clean-up kill, not a timeout: `killed` stays the leader's outcome.
                    0 => signal_group(pgid, false),
                    1 => signal_group(pgid, true),
                    _ => break,
                };
                stage += 1;
                deadline = Instant::now() + KILL_GRACE;
            }
        }
    }
    if killed {
        signal_group(pgid, true);
    }
    out.code = status.and_then(|s| s.code());
    out.elapsed_ms = start.elapsed().as_millis() as u64;
    if cancelled {
        return Err(Cancelled.into());
    }
    Ok(out)
}

/// New process group: `process_group(0)` on Unix; on Windows a new group without a console
/// window, so the tree can be torn down via `taskkill /T`.
fn isolate(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

/// SIGTERM (`force = false`) or SIGKILL to the group led by `pgid`. Windows: `taskkill /T`
/// on the tree rooted at `pgid` (`/F` when forced; unforced only reaches windowed processes).
fn signal_group(pgid: u32, force: bool) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pgid as libc::pid_t, if force { libc::SIGKILL } else { libc::SIGTERM });
    }
    #[cfg(windows)]
    {
        let pid = pgid.to_string();
        let mut args = vec!["/T", "/PID", pid.as_str()];
        if force {
            args.push("/F");
        }
        let _ = Command::new("taskkill")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
}

fn kill_group(pgid: u32, child: &mut std::process::Child, status: &mut Option<std::process::ExitStatus>) {
    signal_group(pgid, false);
    let until = Instant::now() + KILL_GRACE;
    while Instant::now() < until {
        if let Ok(Some(s)) = child.try_wait() {
            *status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    signal_group(pgid, true);
    if status.is_none() {
        *status = child.wait().ok();
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
    use super::*;

    #[test]
    fn captures_output_and_code() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err >&2; exit 3"]);
        let b = run_bounded(c, Duration::from_secs(5)).unwrap();
        assert_eq!(b.stdout, "out\n");
        assert_eq!(b.stderr, "err\n");
        assert_eq!(b.code, Some(3));
        assert!(!b.killed);
    }

    #[test]
    fn stdin_is_piped_when_given() {
        let mut c = Command::new("sh");
        c.args(["-c", "cat"]);
        let b = run_bounded_input(c, Duration::from_secs(5), Some(b"hello")).unwrap();
        assert_eq!(b.stdout, "hello");
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let mut c = Command::new("sh");
        // The grandchild keeps stdout open; the group kill must still end the call.
        c.args(["-c", "sleep 30 & sleep 30"]);
        let t = Instant::now();
        let b = run_bounded(c, Duration::from_millis(200)).unwrap();
        assert!(b.killed);
        assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
    }

    #[cfg(unix)] // POSIX sh fixtures
    #[test]
    fn leader_exit_with_lingering_grandchild_is_bounded() {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30 & echo done"]);
        let t = Instant::now();
        let b = run_bounded(c, Duration::from_secs(20)).unwrap();
        assert_eq!(b.code, Some(0));
        assert!(b.stdout.starts_with("done"));
        assert!(!b.killed, "clean-up of a lingering member is not a timeout");
        assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
    }
}
