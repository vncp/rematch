//! `match --cwd` (load-time payload for the extension factory) and `digest --cwd`
//! (session-start lines, one-tap approvals, capability-floor review, transfers).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use rematch_core::features::{exts_of, stems_of_session};
use rematch_core::ledger::{enum_str, EntryFilter};
use rematch_core::library;
use rematch_core::predicate;
use rematch_core::types::{Entry, Kind, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;

/// Max bytes of file content shown per approval.
pub const APPROVAL_DIFF_BYTES: usize = 4096;

#[derive(Args, Debug, Clone, Default)]
pub struct MatchArgs {
    /// First inputs/paths of the session: a JSON array of texts, or `{"texts":[…],"paths":[…]}`.
    /// Refines `predicate_hit` with the count-time content rule.
    #[arg(long)]
    pub inputs: Option<String>,
}

pub fn cmd_match(ctx: &AppCtx, a: MatchArgs) -> Result<Value> {
    let repo = rematch_core::import::repo_of(&ctx.cwd);
    let m = library::list_for_match(&ctx.ledger, &ctx.paths, repo.as_deref(), &ctx.cwd)?;
    let refine = match &a.inputs {
        Some(raw) => {
            let v: Value = serde_json::from_str(raw).context("--inputs must be JSON")?;
            let strings = |x: Option<&Value>| -> Vec<String> {
                x.and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
            };
            let (texts, paths) = if v.is_array() { (strings(Some(&v)), Vec::new()) } else { (strings(v.get("texts")), strings(v.get("paths"))) };
            Some((exts_of(&paths), stems_of_session(&texts, &paths)))
        }
        None => None,
    };
    let mut entries = Vec::new();
    for me in &m.entries {
        let e = ctx.ledger.get_entry(&me.id)?;
        // Lessons are library records, not loaded artifacts.
        if e.as_ref().is_some_and(|e| e.kind == Kind::Lesson) {
            continue;
        }
        let mut hit = me.predicate_hit;
        if let (Some((exts, stems)), Some(e)) = (&refine, &e) {
            hit = hit && predicate::count_hit(&e.predicate, exts, stems);
        }
        entries.push(json!({
            "id": me.id, "name": me.name, "status": me.status, "predicate_hit": hit,
            "families_excluded": e.map(|e| e.families_excluded).unwrap_or_default(),
        }));
    }
    let mut out = serde_json::to_value(&m)?;
    out["entries"] = Value::Array(entries);
    Ok(out)
}

fn last_path(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join("digest.last")
}

fn status_line(e: &Entry, c: &rematch_core::types::Counters) -> String {
    let counts = format!("matched {} · recurrence {} · harm {}", c.matched, c.recurrence, c.harm);
    let kind = enum_str(&e.kind);
    match e.status {
        Status::Provisional => format!("staged {kind} {} in {}: {}", e.name, e.branch, rematch_core::features::head(&e.trigger_line, 160)),
        Status::Draft => format!("draft {kind} {} (never loads; `/rematch activate {} --unverified|--task <id>`)", e.name, e.name),
        Status::Kept => format!("kept {kind} {} ({counts})", e.name),
        Status::Retired => format!("retired {kind} {} ({}; {counts})", e.name, e.status_note.as_deref().unwrap_or("counters")),
        Status::Dormant => format!("dormant {kind} {} ({counts})", e.name),
        Status::Stale => format!("stale {kind} {} ({})", e.name, e.status_note.as_deref().unwrap_or("evidence drift")),
        Status::Pinned => format!("pinned {kind} {}", e.name),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[+{} bytes]", &s[..end], s.len() - end)
}

/// Provisional/kept entries whose scripts or hook await the one-tap approval.
pub fn pending_approvals(ctx: &AppCtx) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            let reason = e
                .manifest
                .as_ref()
                .map(|m| format!("{} Predicted fix: {}", m.rationale, m.predicted_fix).trim().to_string())
                .unwrap_or_else(|| e.trigger_line.clone());
            if e.kind == Kind::Hook {
                if !ctx.paths.hooks_dir().join(format!("{}.ts", e.id)).is_file() {
                    let diff = match library::hook_module(&e) {
                        Ok(m) => m,
                        Err(err) => format!("(hook does not compile: {err:#})"),
                    };
                    out.push(json!({"id": e.id, "name": e.name, "kind": "hook", "diff": truncate(&diff, APPROVAL_DIFF_BYTES), "reason": reason}));
                }
                continue;
            }
            let scripts_dir = e.path.join("scripts");
            if !scripts_dir.is_dir() || !crate::reflect::owns_dir(ctx, &e) {
                continue;
            }
            let mut files: Vec<PathBuf> = walk(&scripts_dir);
            files.sort();
            let unapproved: Vec<(String, PathBuf)> = files
                .into_iter()
                .map(|p| (p.strip_prefix(&e.path).unwrap_or(&p).to_string_lossy().replace('\\', "/"), p))
                .filter(|(rel, _)| !e.approved_scripts.contains(rel))
                .collect();
            if unapproved.is_empty() {
                continue;
            }
            let mut diff = String::new();
            for (rel, p) in &unapproved {
                diff.push_str(&format!("+++ {rel}\n{}\n", std::fs::read_to_string(p).unwrap_or_default()));
            }
            out.push(json!({"id": e.id, "name": e.name, "kind": "scripts", "diff": truncate(&diff, APPROVAL_DIFF_BYTES), "reason": reason}));
        }
    }
    Ok(out)
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

pub fn cmd_digest(ctx: &AppCtx) -> Result<Value> {
    let last: i64 = std::fs::read_to_string(last_path(ctx)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let mut lines = Vec::new();
    let mut changed = Vec::new();
    let mut registered = 0usize;
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        let since = e.status_since.unwrap_or(e.created_at);
        if since <= last {
            continue;
        }
        changed.push(json!({"id": e.id, "name": e.name, "status": e.status, "since": since}));
        // Imported registrations (forge/brownfield, files left in place) were not staged:
        // one summary line instead of one per entry.
        if e.status == Status::Provisional && matches!(e.origin, rematch_core::types::Origin::Import { .. }) && e.version <= 1 && e.sibling_of.is_none() && !e.path.starts_with(&ctx.paths.staging) {
            registered += 1;
            continue;
        }
        let c = ctx.ledger.counters(&e.id)?;
        lines.push(status_line(&e, &c));
    }
    if registered > 0 {
        lines.push(format!("registered {registered} imported entr{} for measurement (files left in place)", if registered == 1 { "y" } else { "ies" }));
    }
    let mut approvals = pending_approvals(ctx)?;
    for x in crate::scope::export::pending(ctx)? {
        let mut v = serde_json::to_value(&x)?;
        if let Some(id) = v.get("id").and_then(Value::as_str).map(str::to_string) {
            if !id.starts_with("export:") {
                v["id"] = json!(format!("export:{id}"));
            }
        }
        approvals.push(v);
    }
    let review: Vec<Value> =
        crate::scope::split::capability_review(ctx, 7)?.into_iter().map(|r| serde_json::to_value(&r)).collect::<Result<_, _>>()?;
    for r in &review {
        lines.push(format!(
            "review: family {} rose across {} entries (recurrence {:+}, harm {:+}) in the last 7 days",
            r.get("family").and_then(Value::as_str).unwrap_or("?"),
            r.get("entries_affected").map(|v| v.to_string()).unwrap_or_default(),
            r.get("recurrence_delta").and_then(Value::as_i64).unwrap_or(0),
            r.get("harm_delta").and_then(Value::as_i64).unwrap_or(0),
        ));
    }
    let stale: Vec<Value> =
        crate::scope::stale::check(ctx)?.into_iter().map(|s| serde_json::to_value(&s)).collect::<Result<_, _>>()?;
    for s in &stale {
        lines.push(format!(
            "stale: {} ({})",
            s.get("name").and_then(Value::as_str).unwrap_or("?"),
            s.get("reason").and_then(Value::as_str).unwrap_or("drift")
        ));
    }
    let transfers: Vec<Value> =
        crate::scope::transfer::suggest(ctx)?.into_iter().map(|s| serde_json::to_value(&s)).collect::<Result<_, _>>()?;
    for t in &transfers {
        lines.push(format!(
            "transfer suggestion: {} → {} (`/rematch transfer apply {} --branch {}`)",
            t.get("name").and_then(Value::as_str).unwrap_or("?"),
            t.get("to_branch").and_then(Value::as_str).unwrap_or("?"),
            t.get("lesson_id").and_then(Value::as_str).unwrap_or("?"),
            t.get("to_branch").and_then(Value::as_str).unwrap_or("?"),
        ));
    }
    if !approvals.is_empty() {
        lines.push(format!("{} approval(s) pending", approvals.len()));
    }
    std::fs::write(last_path(ctx), ctx.now.to_string())?;
    Ok(json!({
        "since": last,
        "lines": lines,
        "changed": changed,
        "approvals": approvals,
        "review": review,
        "stale": stale,
        "transfers": transfers,
        "off": ctx.paths.is_off(),
    }))
}
