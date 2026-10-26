//! Detached jobs (DESIGN §14): the binary re-executes itself in a new session
//! (`setsid`), stdio → `jobs/<id>.log`, state in `jobs/<id>.json` mirrored to the ledger.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

use crate::app::{new_id, AppCtx};

pub const JOB_ID_TOKEN: &str = "{job}";
pub const JOB_ID_ENV: &str = "OMP_EVOLVE_JOB_ID";

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
/// child also gets `OMP_EVOLVE_JOB_ID`. `payload` (an object) is merged into the job
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
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn detached job")?;
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
    rec.insert("updated_at".into(), json!(evolve_core::ledger::now_ms()));
    let v = Value::Object(rec);
    write_json(ctx, id, &v)?;
    Ok(v)
}

pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
        *by_status.entry(evolve_core::ledger::enum_str(&e.status)).or_default() += 1;
    }
    lines.insert(0, format!("entries: {}", by_status.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")));
    Ok(json!({"jobs": jobs, "entries": by_status, "off": ctx.paths.is_off(), "lines": lines}))
}

/// Exclusive lock file with stale-pid takeover (`pending/.lock` for reflect).
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    /// `Ok(None)` when another live process holds it.
    pub fn acquire(path: PathBuf) -> Result<Option<LockFile>> {
        use std::io::Write;
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    writeln!(f, "{}", std::process::id())?;
                    return Ok(Some(LockFile { path }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i64>().ok());
                    match holder {
                        Some(pid) if pid_alive(pid) && pid != std::process::id() as i64 => return Ok(None),
                        None if recently_modified(&path) => return Ok(None), // holder still writing its pid
                        _ => {
                            // Stale (dead holder or unreadable): take it over.
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
        Ok(None)
    }
}

fn recently_modified(p: &std::path::Path) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(5))
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_stale_locks_are_taken_over() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(".lock");
        let a = LockFile::acquire(p.clone()).unwrap().expect("first holder");
        assert!(p.exists());
        // A live foreign holder: pid 1 (launchd/init) is always alive.
        drop(a);
        std::fs::write(&p, "1\n").unwrap();
        assert!(LockFile::acquire(p.clone()).unwrap().is_none(), "live holder blocks");
        // A dead holder's lock is taken over.
        std::fs::write(&p, "999999999\n").unwrap();
        let b = LockFile::acquire(p.clone()).unwrap().expect("stale lock taken over");
        assert!(p.exists());
        drop(b);
        assert!(!p.exists(), "released on drop");
    }
}
