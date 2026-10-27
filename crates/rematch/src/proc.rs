//! Bounded process execution with process-tree kill (DESIGN §14). Shared by the
//! editor/classifier channel, batch rollouts and verifiers. The tree is the child's own
//! process group on Unix and a job object on Windows.

use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
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

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    CANCEL.store(true, Ordering::SeqCst);
}

/// SIGINT/SIGTERM set the cancel flag instead of killing the binary, so a running child
/// group is torn down and the envelope reports `cancelled` (exit 130).
#[cfg(unix)]
pub fn install_cancel_handler() {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Ctrl+C / Ctrl+Break set the cancel flag instead of ending the binary, so a running child
/// tree is torn down and the envelope reports `cancelled` (exit 130). Close, logoff and
/// shutdown keep their default (the process ends; its job objects close and kill the trees).
#[cfg(windows)]
pub fn install_cancel_handler() {
    use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
    unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
        match kind {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                CANCEL.store(true, Ordering::SeqCst);
                1
            }
            _ => 0,
        }
    }
    // SAFETY: registers a handler that only stores to an atomic.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
}

pub fn cancel_requested() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/// Request cancellation (tests, internal aborts).
pub fn request_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Spawns `cmd` as its own process tree ([`Tree`]), stdin null, stdout/stderr piped
/// (bounded buffers), waits ≤ `timeout`; on timeout or cancel fires SIGTERM to the tree,
/// waits 2 s, SIGKILL to the tree (even if the leader exited).
pub fn run_bounded(cmd: Command, timeout: Duration) -> Result<Bounded> {
    run_bounded_input(cmd, timeout, None)
}

/// [`run_bounded`] with optional bytes written to the child's stdin (scripted editors).
pub fn run_bounded_input(mut cmd: Command, timeout: Duration, input: Option<&[u8]>) -> Result<Bounded> {
    let start = Instant::now();
    // A detached job's id must not leak into tools it runs (they would report into it).
    cmd.env_remove(crate::jobs::JOB_ID_ENV);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = format!("{:?}", cmd.get_program());
    let (mut child, tree) = Tree::spawn(&mut cmd).with_context(|| format!("spawn {program}"))?;

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
            kill_tree(&tree, &mut child, &mut status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Collect the streams. A tree member still holding the pipes after the leader is
    // gone gets SIGTERM, then SIGKILL; after that round the pipes are abandoned (a member
    // that left the tree cannot be signalled through it).
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
                    0 => tree.signal(false),
                    1 => tree.signal(true),
                    _ => break,
                };
                stage += 1;
                deadline = Instant::now() + KILL_GRACE;
            }
        }
    }
    if killed {
        tree.signal(true);
    }
    out.code = status.and_then(|s| s.code());
    out.elapsed_ms = start.elapsed().as_millis() as u64;
    if cancelled {
        return Err(Cancelled.into());
    }
    Ok(out)
}

/// The spawned command and everything it starts. Unix: its own process group
/// (`process_group(0)`); a member leaves only through `setsid`. Windows: a job object the
/// child joins before its first instruction (spawned suspended, resumed once assigned), with
/// no console window; a member leaves only by asking for breakaway. Closing the job (this
/// value dropped, or this process ending however it ends) kills what is still in it.
pub(crate) struct Tree {
    #[cfg(unix)]
    pgid: libc::pid_t,
    #[cfg(windows)]
    job: win::Job,
}

impl Tree {
    #[cfg(unix)]
    pub(crate) fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Tree)> {
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pgid = child.id() as libc::pid_t;
        Ok((child, Tree { pgid }))
    }

    #[cfg(windows)]
    pub(crate) fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Tree)> {
        use windows_sys::Win32::System::JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
        Self::spawn_in_job(cmd, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK)
    }

    /// [`Tree::spawn`] into a job with these `JOB_OBJECT_LIMIT_*` flags (tests use others).
    #[cfg(windows)]
    pub(crate) fn spawn_in_job(cmd: &mut Command, limits: u32) -> std::io::Result<(Child, Tree)> {
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED};
        cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        let mut child = cmd.spawn()?;
        match win::Job::adopt(&child, limits) {
            Ok(job) => Ok((child, Tree { job })),
            Err(e) => {
                // Never leave a suspended child behind.
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// Unix: SIGTERM (`force = false`) or SIGKILL to the group. Windows: a windowless
    /// console tree has no termination request it can catch, so both terminate the job.
    pub(crate) fn signal(&self, force: bool) {
        #[cfg(unix)]
        // SAFETY: killpg only sends a signal.
        unsafe {
            libc::killpg(self.pgid, if force { libc::SIGKILL } else { libc::SIGTERM });
        }
        #[cfg(windows)]
        {
            let _ = force;
            self.job.terminate();
        }
    }
}

fn kill_tree(tree: &Tree, child: &mut Child, status: &mut Option<std::process::ExitStatus>) {
    tree.signal(false);
    let until = Instant::now() + KILL_GRACE;
    while Instant::now() < until {
        if let Ok(Some(s)) = child.try_wait() {
            *status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    tree.signal(true);
    if status.is_none() {
        *status = child.wait().ok();
    }
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// A job object; closing it kills its remaining members when it has
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    pub(super) struct Job(OwnedHandle);

    fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a valid handle just returned to us and owned by nobody else.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Job {
        /// A new job with the `limits` (`JOB_OBJECT_LIMIT_*`) holding the suspended `child`,
        /// which is then resumed: nothing it starts runs outside the job unless it breaks away.
        pub(super) fn adopt(child: &Child, limits: u32) -> io::Result<Job> {
            // SAFETY: Win32 calls on handles owned here or by `child`, with pointers to locals
            // that outlive each call.
            unsafe {
                let job = owned(CreateJobObjectW(std::ptr::null(), std::ptr::null()))?;
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = limits;
                check(SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ))?;
                check(AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()))?;
                resume(child.id())?;
                Ok(Job(job))
            }
        }

        /// Every member ends (exit code 1). Fails only on a closed handle, which this
        /// value never holds.
        pub(super) fn terminate(&self) {
            // SAFETY: the job handle is owned by `self`.
            unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
        }
    }

    /// Resumes the threads of the suspended process `pid` (a new process has one).
    fn resume(pid: u32) -> io::Result<()> {
        // SAFETY: Win32 calls on handles owned here, with a correctly sized THREADENTRY32.
        unsafe {
            let snap = owned(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0))?;
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0;
            let mut more = Thread32First(snap.as_raw_handle(), &mut e) != 0;
            while more {
                if e.th32OwnerProcessID == pid {
                    let t = owned(OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID))?;
                    if ResumeThread(t.as_raw_handle()) == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    resumed += 1;
                }
                more = Thread32Next(snap.as_raw_handle(), &mut e) != 0;
            }
            if resumed == 0 {
                return Err(io::Error::other(format!("no thread of process {pid} to resume")));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test binary is the child process (Windows has no `sh`): [`fixture`] acts out
    /// the mode named here and does nothing without it.
    const MODE: &str = "REMATCH_PROC_FIXTURE";
    /// File the `tree-*` modes write their grandchild's pid to.
    const PID_FILE: &str = "REMATCH_PROC_FIXTURE_PID";

    fn fixture_cmd(mode: &str) -> Command {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["proc::tests::fixture", "--exact", "--ignored", "--nocapture", "--test-threads=1"]).env(MODE, mode);
        c
    }

    #[test]
    #[ignore = "child process of the tests below"]
    fn fixture() {
        let Ok(mode) = std::env::var(MODE) else { return };
        let long = Duration::from_secs(60);
        match mode.as_str() {
            "echo" => {
                println!("out");
                eprintln!("err");
                std::process::exit(3);
            }
            "cat" => {
                std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
                std::io::stdout().flush().unwrap();
            }
            "sleep" => std::thread::sleep(long),
            "tree-wait" | "tree-exit" => {
                // The grandchild inherits stdout and stderr: it holds the pipes, and outlives
                // this process unless the tree kill reaches it.
                #[allow(clippy::zombie_processes)]
                let g = fixture_cmd("sleep").env_remove(PID_FILE).spawn().unwrap();
                std::fs::write(std::env::var(PID_FILE).unwrap(), g.id().to_string()).unwrap();
                if mode == "tree-exit" {
                    println!("done");
                } else {
                    std::thread::sleep(long);
                }
            }
            other => panic!("unknown fixture mode {other}"),
        }
        std::process::exit(0);
    }

    /// The grandchild the fixture recorded, gone within 10 s.
    fn assert_gone(pid_file: &std::path::Path) {
        let pid: i64 = std::fs::read_to_string(pid_file).expect("grandchild started").trim().parse().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while crate::jobs::pid_alive(pid) {
            assert!(Instant::now() < until, "grandchild {pid} outlived its tree");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn captures_output_and_code() {
        let b = run_bounded(fixture_cmd("echo"), Duration::from_secs(30)).unwrap();
        assert!(b.stdout.ends_with("out\n"), "{:?}", b.stdout);
        assert_eq!(b.stderr, "err\n");
        assert_eq!(b.code, Some(3));
        assert!(!b.killed);
    }

    #[test]
    fn stdin_is_piped_when_given() {
        let b = run_bounded_input(fixture_cmd("cat"), Duration::from_secs(30), Some(b"hello")).unwrap();
        assert!(b.stdout.ends_with("hello"), "{:?}", b.stdout);
    }

    #[test]
    fn timeout_kills_the_whole_tree() {
        let t = tempfile::tempdir().unwrap();
        let pid_file = t.path().join("grandchild");
        let mut c = fixture_cmd("tree-wait");
        c.env(PID_FILE, &pid_file);
        let start = Instant::now();
        // The grandchild keeps stdout open; the tree kill must still end the call.
        let b = run_bounded(c, Duration::from_secs(3)).unwrap();
        assert!(b.killed);
        assert!(start.elapsed() < Duration::from_secs(20), "took {:?}", start.elapsed());
        assert_gone(&pid_file);
    }

    #[test]
    fn leader_exit_with_lingering_grandchild_is_bounded() {
        let t = tempfile::tempdir().unwrap();
        let pid_file = t.path().join("grandchild");
        let mut c = fixture_cmd("tree-exit");
        c.env(PID_FILE, &pid_file);
        let start = Instant::now();
        let b = run_bounded(c, Duration::from_secs(30)).unwrap();
        assert_eq!(b.code, Some(0));
        assert!(b.stdout.contains("done\n"), "{:?}", b.stdout);
        assert!(!b.killed, "clean-up of a lingering member is not a timeout");
        assert!(start.elapsed() < Duration::from_secs(20), "took {:?}", start.elapsed());
        assert_gone(&pid_file);
    }

    #[cfg(windows)]
    #[test]
    fn batch_script_in_a_spaced_path_gets_its_arguments() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("dir with space");
        std::fs::create_dir(&dir).unwrap();
        let script = dir.join("echo args.cmd");
        std::fs::write(&script, "@echo off\r\necho [%~1] [%~2]\r\n").unwrap();
        let mut c = Command::new(&script);
        c.args(["a b", "c"]);
        let b = run_bounded(c, Duration::from_secs(30)).unwrap();
        assert_eq!((b.code, b.stdout.as_str()), (Some(0), "[a b] [c]\r\n"), "{}", b.stderr);
    }
}
