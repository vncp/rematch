//! Detached jobs (DESIGN §14): the binary re-executes itself in a new session
//! (`setsid`; Windows: detached, new process group, out of any job that allows it), stdio →
//! `jobs/<id>.log`, state in `jobs/<id>.json` mirrored to the ledger.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

use crate::app::{new_id, AppCtx};

pub const JOB_ID_TOKEN: &str = "{job}";
pub const JOB_ID_ENV: &str = "REMATCH_JOB_ID";

fn json_path(ctx: &AppCtx, id: &str) -> PathBuf {
    ctx.paths.jobs.join(format!("{id}.json"))
}

pub fn log_path(ctx: &AppCtx, id: &str) -> PathBuf {
    ctx.paths.jobs.join(format!("{id}.log"))
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(anyhow!("invalid job id {id:?}"));
    }
    Ok(())
}

fn write_json(ctx: &AppCtx, id: &str, v: &Value) -> Result<()> {
    let p = json_path(ctx, id);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    std::fs::rename(&tmp, &p)?;
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("job");
    let status = v.get("status").and_then(Value::as_str).unwrap_or("unknown");
    ctx.ledger.put_job(id, kind, status, v)
}

/// Spawn `current_exe argv…` detached. Tokens equal to `{job}` become the new id; the
/// child also gets `REMATCH_JOB_ID`. `payload` (an object) is merged into the job
/// record before the child starts. Returns the job id.
pub fn spawn_detached(ctx: &AppCtx, kind: &str, argv: &[String], payload: &Value) -> Result<String> {
    let id = new_id();
    let argv: Vec<String> = argv.iter().map(|a| if a == JOB_ID_TOKEN { id.clone() } else { a.clone() }).collect();
    let mut rec = Map::new();
    if let Value::Object(p) = payload {
        rec.extend(p.clone());
    }
    rec.insert("id".into(), json!(id));
    rec.insert("kind".into(), json!(kind));
    rec.insert("status".into(), json!("starting"));
    rec.insert("pid".into(), Value::Null);
    rec.insert("started_at".into(), json!(ctx.now));
    rec.insert("argv".into(), json!(argv));
    rec.insert("log".into(), json!(log_path(ctx, &id)));
    write_json(ctx, &id, &Value::Object(rec.clone()))?;

    let exe = std::env::current_exe().context("current exe")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path(ctx, &id))?;
    let mut cmd = Command::new(exe);
    cmd.args(&argv)
        .current_dir(&ctx.cwd)
        .env(JOB_ID_ENV, &id)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(unix)]
    let spawned = cmd.spawn();
    #[cfg(windows)]
    let spawned = spawn_out_of_job(&mut cmd);
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let e = anyhow::Error::from(e).context("spawn detached job");
            let _ = update(ctx, &id, "failed", &json!({"error": format!("{e:#}")}));
            return Err(e);
        }
    };
    let pid = child.id();
    // The child is reparented when we exit; we never wait on it.
    std::mem::forget(child);
    rec.insert("pid".into(), json!(pid));
    rec.insert("status".into(), json!("running"));
    // The child may already have updated its record; merge rather than clobber.
    let current = read(ctx, &id).unwrap_or_else(|_| Value::Object(rec.clone()));
    let mut merged = current.as_object().cloned().unwrap_or_default();
    merged.insert("pid".into(), json!(pid));
    if merged.get("status").and_then(Value::as_str) == Some("starting") {
        merged.insert("status".into(), json!("running"));
    }
    write_json(ctx, &id, &Value::Object(merged))?;
    Ok(id)
}

/// Windows: the worker leaves the spawner's job too (as `setsid` leaves its process
/// group), so a launcher's job that kills its members when it closes (libuv's, for one)
/// does not take the worker with it. A job that forbids breakaway refuses that; the worker
/// then starts inside it only when that job does not kill on close (it outlives the
/// launcher there), and otherwise is not started at all. Only the spawner's own job can be
/// inspected: an enclosing job of a nested one is not.
#[cfg(windows)]
fn spawn_out_of_job(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if own_job_kills_on_close()? {
                return Err(std::io::Error::other(
                    "this process's job forbids breakaway and kills its members when it closes, so a detached job would end with its launcher; start rematch outside that job",
                ));
            }
            cmd.creation_flags(flags).spawn()
        }
        r => r,
    }
}

/// The job this process belongs to ends its members when its last handle closes.
#[cfg(windows)]
fn own_job_kills_on_close() -> std::io::Result<bool> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectExtendedLimitInformation, QueryInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    // SAFETY: a null handle names this process's job; the buffer is a local of the size passed.
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0)
    }
}

pub fn read(ctx: &AppCtx, id: &str) -> Result<Value> {
    valid_id(id)?;
    let p = json_path(ctx, id);
    let text = std::fs::read_to_string(&p).with_context(|| format!("no job {id}"))?;
    Ok(serde_json::from_str(&text)?)
}

/// Shallow-merge `patch` into the record and set `status`; mirrored to the ledger.
pub fn update(ctx: &AppCtx, id: &str, status: &str, patch: &Value) -> Result<Value> {
    let mut rec = read(ctx, id)?.as_object().cloned().unwrap_or_default();
    if let Value::Object(p) = patch {
        rec.extend(p.clone());
    }
    rec.insert("status".into(), json!(status));
    rec.insert("updated_at".into(), json!(rematch_core::ledger::now_ms()));
    let v = Value::Object(rec);
    write_json(ctx, id, &v)?;
    Ok(v)
}

/// The process exists (signal 0 delivered, or refused for permission).
#[cfg(unix)]
pub fn pid_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The process exists and has not exited (or exists and refuses to be opened).
#[cfg(windows)]
pub fn pid_alive(pid: i64) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let Ok(pid) = u32::try_from(pid) else { return false };
    if pid == 0 {
        return false;
    }
    // SAFETY: Win32 calls; the handle is owned (and closed) here.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let h = OwnedHandle::from_raw_handle(h);
        WaitForSingleObject(h.as_raw_handle(), 0) == WAIT_TIMEOUT
    }
}

const LIVE: &[&str] = &["starting", "running"];

/// Job record with liveness: a live status whose pid is gone becomes `lost`.
pub fn status(ctx: &AppCtx, id: &str) -> Result<Value> {
    let mut v = read(ctx, id)?;
    let st = v.get("status").and_then(Value::as_str).unwrap_or("").to_string();
    let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let alive = pid_alive(pid);
    v["alive"] = json!(alive);
    if LIVE.contains(&st.as_str()) && pid > 0 && !alive {
        v = update(ctx, id, "lost", &json!({"note": "process exited without recording a final status"}))?;
        v["alive"] = json!(false);
    }
    Ok(v)
}

/// `status [job]`: one job, or every job newest first.
pub fn cmd_status(ctx: &AppCtx, job: Option<&str>) -> Result<Value> {
    if let Some(id) = job {
        let v = status(ctx, id)?;
        let line = format!(
            "{} {} {}",
            v.get("kind").and_then(Value::as_str).unwrap_or("job"),
            id,
            v.get("status").and_then(Value::as_str).unwrap_or("?")
        );
        let mut out = v;
        out["lines"] = json!([line]);
        return Ok(out);
    }
    let mut ids: Vec<String> = std::fs::read_dir(&ctx.paths.jobs)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string))
        .collect();
    ids.sort();
    ids.reverse();
    let mut jobs = Vec::new();
    let mut lines = Vec::new();
    for id in ids {
        if let Ok(v) = status(ctx, &id) {
            lines.push(format!(
                "{} {} {}",
                v.get("kind").and_then(Value::as_str).unwrap_or("job"),
                id,
                v.get("status").and_then(Value::as_str).unwrap_or("?")
            ));
            jobs.push(v);
        }
    }
    let entries = ctx.ledger.list_entries(&Default::default())?;
    let mut by_status = std::collections::BTreeMap::<String, u32>::new();
    for e in &entries {
        *by_status.entry(rematch_core::ledger::enum_str(&e.status)).or_default() += 1;
    }
    lines.insert(0, format!("entries: {}", by_status.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")));
    Ok(json!({"jobs": jobs, "entries": by_status, "off": ctx.paths.is_off(), "lines": lines}))
}

/// Exclusive OS lock on a lock file (`pending/.lock` for reflect): `flock` on Unix,
/// `LockFileEx` on Windows. The OS releases it when the value drops or the holder dies
/// however it dies, so there is no stale holder to detect and no pid to trust. The file
/// (the holder's pid, for people) stays: removing it would let a waiter that opened the old
/// file and a newcomer that creates a new one both hold "the" lock.
pub struct LockFile {
    file: std::fs::File,
}

impl LockFile {
    /// `Ok(None)` when another holder has it.
    pub fn acquire(path: PathBuf) -> Result<Option<LockFile>> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e).with_context(|| format!("lock {}", path.display())),
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Some(LockFile { file }))
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_until_dropped_and_a_dead_holders_file_is_free() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(".lock");
        // What a crashed holder leaves: the file, its pid, no lock.
        std::fs::write(&p, "999999999\n").unwrap();
        let a = LockFile::acquire(p.clone()).unwrap().expect("a dead holder's file is free");
        assert!(LockFile::acquire(p.clone()).unwrap().is_none(), "a live holder blocks");
        drop(a);
        let b = LockFile::acquire(p.clone()).unwrap().expect("released on drop");
        drop(b);
    }

    #[test]
    fn liveness_follows_the_process() {
        assert!(pid_alive(std::process::id() as i64));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
        assert!(!pid_alive(i64::MAX));
        let child = Command::new(std::env::current_exe().unwrap()).arg("--list").stdout(Stdio::null()).spawn().unwrap();
        let pid = child.id() as i64;
        child.wait_with_output().unwrap();
        assert!(!pid_alive(pid), "an exited, reaped child is gone");
    }

    /// Set to a root, [`detach_fixture`] is a launcher calling [`spawn_detached`] (which
    /// re-runs this test binary as the worker) or, with [`JOB_ID_ENV`], that worker.
    const FIXTURE_ROOT: &str = "REMATCH_DETACH_FIXTURE_ROOT";
    const FIXTURE_ARGS: [&str; 4] = ["jobs::tests::detach_fixture", "--exact", "--ignored", "--test-threads=1"];

    fn fixture_ctx(root: &std::path::Path) -> AppCtx {
        let paths = rematch_core::paths::RematchPaths::with_roots(root.join("rematch"), root.join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = rematch_core::ledger::Ledger::open(&paths.ledger_file).unwrap();
        AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: root.to_path_buf(), text: false }
    }

    #[test]
    #[ignore = "child process of the detach tests below"]
    fn detach_fixture() {
        let Some(root) = std::env::var_os(FIXTURE_ROOT).map(PathBuf::from) else { return };
        let ctx = fixture_ctx(&root);
        match std::env::var(JOB_ID_ENV) {
            Ok(id) => {
                // Long enough for the test to tear the launcher's tree down first.
                std::thread::sleep(std::time::Duration::from_secs(2));
                update(&ctx, &id, "done", &json!({"worker_pid": std::process::id()})).unwrap();
            }
            Err(_) => {
                let argv = FIXTURE_ARGS.map(String::from);
                let outcome = match spawn_detached(&ctx, "fixture", &argv, &json!({})) {
                    Ok(id) => format!("ok {id}"),
                    Err(e) => format!("err {e:#}"),
                };
                std::fs::write(root.join("launched"), outcome).unwrap();
            }
        }
    }

    /// Runs the launcher fixture to its exit in the tree `spawn` makes; its outcome line.
    fn launch(
        root: &std::path::Path,
        spawn: impl FnOnce(&mut Command) -> std::io::Result<(std::process::Child, crate::proc::Tree)>,
    ) -> (String, crate::proc::Tree) {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(FIXTURE_ARGS).env(FIXTURE_ROOT, root).env_remove(JOB_ID_ENV).stdout(Stdio::null()).stderr(Stdio::null());
        let (mut child, tree) = spawn(&mut c).unwrap();
        assert!(child.wait().unwrap().success(), "launcher failed");
        (std::fs::read_to_string(root.join("launched")).unwrap(), tree)
    }

    /// The job's record once it left `starting`/`running`, with liveness (a killed worker
    /// is `lost`), within 30 s.
    fn final_record(root: &std::path::Path, id: &str) -> Value {
        let ctx = fixture_ctx(root);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let rec = status(&ctx, id).unwrap();
            if !LIVE.contains(&rec["status"].as_str().unwrap_or_default()) {
                return rec;
            }
            assert!(std::time::Instant::now() < until, "worker never finished: {rec}");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[test]
    fn detached_worker_outlives_its_launchers_tree() {
        let t = tempfile::tempdir().unwrap();
        // Unix: the launcher's own process group. Windows: a job that kills its members on
        // close and lets them break away (libuv's, which Bun and Node spawn into).
        let (outcome, tree) = launch(t.path(), crate::proc::Tree::spawn);
        // The launcher has exited; what is left of its tree goes now, as a session's would
        // (Windows: closing the job handle at the end of this scope kills again).
        tree.signal(true);
        let id = outcome.strip_prefix("ok ").unwrap_or_else(|| panic!("{outcome}"));
        let rec = final_record(t.path(), id);
        assert_eq!(rec["status"], "done", "{rec}");
        assert_eq!(rec["worker_pid"], rec["pid"], "{rec}");
    }

    #[cfg(windows)]
    #[test]
    fn detaching_from_a_kill_on_close_job_that_forbids_breakaway_is_refused() {
        use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let t = tempfile::tempdir().unwrap();
        let (outcome, _tree) = launch(t.path(), |c| crate::proc::Tree::spawn_in_job(c, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE));
        assert!(outcome.starts_with("err ") && outcome.contains("forbids breakaway"), "{outcome}");
        let ctx = fixture_ctx(t.path());
        let ids: Vec<String> = std::fs::read_dir(&ctx.paths.jobs)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string))
            .collect();
        let [id] = &ids[..] else { panic!("one job record: {ids:?}") };
        let rec = status(&ctx, id).unwrap();
        assert_eq!(rec["status"], "failed", "never reported as running: {rec}");
        assert!(rec["error"].as_str().unwrap().contains("forbids breakaway"), "{rec}");
    }

    #[cfg(windows)]
    #[test]
    fn detached_worker_stays_in_a_job_that_neither_kills_nor_lets_go() {
        let t = tempfile::tempdir().unwrap();
        let (outcome, tree) = launch(t.path(), |c| crate::proc::Tree::spawn_in_job(c, 0));
        // The launcher's job handle closes; without kill-on-close its members live on.
        drop(tree);
        let id = outcome.strip_prefix("ok ").unwrap_or_else(|| panic!("{outcome}"));
        assert_eq!(final_record(t.path(), id)["status"], "done");
    }
}
