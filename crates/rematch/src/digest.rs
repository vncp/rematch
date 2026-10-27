//! `match --cwd` (load-time payload for the extension factory: entries, predicate hits, hook
//! modules, approved prompt fragments) and `digest --cwd` (session-start lines, one-tap
//! approvals, capability-floor review, transfers, ship candidates). Skill delivery is
//! `skills sync` (`skills_cmd`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use rematch_core::features::{exts_of, stems_of_session};
use rematch_core::ledger::{enum_str, EntryFilter};
use rematch_core::domain;
use rematch_core::library::{self, Approval, AGENTS_FRAGMENT, AGENT_MD};
use rematch_core::predicate;
use rematch_core::types::{Entry, Kind, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::scope::ship::ShipReady;

/// Max bytes of file content shown per approval.
pub const APPROVAL_DIFF_BYTES: usize = 4096;

#[derive(Args, Debug, Clone, Default)]
pub struct MatchArgs {
    /// First inputs/paths of the session: a JSON array of texts, or `{"texts":[…],"paths":[…]}`.
    /// Refines `predicate_hit` with the count-time content rule.
    #[arg(long)]
    pub inputs: Option<String>,
}

/// `match --cwd`: the load-time payload. `domain` is the place's declared domain
/// (`policy.json`, else the branch's `domain.yml`); with `--inputs` the session's exts and
/// stems are known too, so the full rule (`domain::domain_of`, inference included) decides
/// it and gates `predicate_hit` like `count` will. `prompt_fragments` (`[{id, name, text}]`,
/// [`library::prompt_fragments`]) are the approved `AGENTS.fragment.md` texts of the listed
/// entries that hit, which harnesses append to a main session's system prompt.
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
    let domain = match &refine {
        Some((exts, stems)) if !m.off => {
            let place = predicate::Place::locate(repo.as_deref(), &ctx.cwd);
            let policy = ctx.domain_policy()?;
            domain::domain_of(place.remote.as_deref(), place.subpath.as_deref(), Some(&m.branch), exts, stems, &policy)
        }
        _ => m.domain.clone(),
    };
    let mut entries = Vec::new();
    let mut hits = Vec::new();
    for me in &m.entries {
        let e = ctx.ledger.get_entry(&me.id)?;
        // Lessons are library records, not loaded artifacts.
        if e.as_ref().is_some_and(|e| e.kind == Kind::Lesson) {
            continue;
        }
        let mut hit = me.predicate_hit;
        if let (Some((exts, stems)), Some(e)) = (&refine, &e) {
            hit = hit
                && predicate::count_hit(&e.predicate, exts, stems)
                && domain::domain_matches(e.predicate.domain.as_deref(), domain.as_deref());
        }
        if hit {
            hits.push(library::MatchedEntry { predicate_hit: true, ..me.clone() });
        }
        entries.push(json!({
            "id": me.id, "name": me.name, "kind": me.kind, "status": me.status, "predicate_hit": hit,
            "version": me.version, "sha": me.sha,
            "families_excluded": e.map(|e| e.families_excluded).unwrap_or_default(),
        }));
    }
    let mut out = serde_json::to_value(&m)?;
    out["entries"] = Value::Array(entries);
    out["domain"] = json!(domain);
    out["prompt_fragments"] = serde_json::to_value(library::prompt_fragments(&ctx.ledger, &ctx.paths, &hits)?)?;
    Ok(out)
}

fn last_path(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join("digest.last")
}

/// `<id>:<scope>` keys of the ship candidates a digest already announced (one per line).
fn ship_shown_path(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join("digest.ship")
}

fn ship_key(r: &ShipReady) -> String {
    format!("{}:{}", r.id, r.scope)
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

/// Provisional, kept and pinned entries with parts awaiting the one-tap approval
/// ([`library::pending_parts`]): one item per entry whose `kind` is its parts joined with `+`
/// (`scripts+prompt`) and whose `diff` holds each part in that order — the unapproved script
/// files, the compiled hook module, `AGENT.md`, `AGENTS.fragment.md` — truncated to
/// [`APPROVAL_DIFF_BYTES`]. `library approve` grants every part at once.
pub fn pending_approvals(ctx: &AppCtx) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            let parts = library::pending_parts(&ctx.paths, &e);
            if parts.is_empty() {
                continue;
            }
            let mut reason = e
                .manifest
                .as_ref()
                .map(|m| format!("{} Predicted fix: {}", m.rationale, m.predicted_fix).trim().to_string())
                .unwrap_or_else(|| e.trigger_line.clone());
            let file = |name: &str| format!("+++ {name}\n{}\n", std::fs::read_to_string(e.path.join(name)).unwrap_or_default());
            let mut diff = String::new();
            for part in &parts {
                match part {
                    Approval::Scripts => {
                        for (rel, p) in library::unapproved_scripts(&e) {
                            diff.push_str(&format!("+++ {rel}\n{}\n", std::fs::read_to_string(p).unwrap_or_default()));
                        }
                    }
                    Approval::Hook => {
                        diff.push_str(&match library::hook_module(&e) {
                            Ok(m) => m,
                            Err(err) => format!("(hook does not compile: {err:#})\n"),
                        });
                        if let Some(h) = &e.hook {
                            reason = format!("{reason}\n\nPass condition: {}", h.pass_condition.trim());
                        }
                    }
                    Approval::Agent => diff.push_str(&file(AGENT_MD)),
                    Approval::Prompt => diff.push_str(&file(AGENTS_FRAGMENT)),
                }
            }
            out.push(json!({
                "id": e.id, "name": e.name, "kind": library::parts_label(&parts),
                "diff": truncate(&diff, APPROVAL_DIFF_BYTES), "reason": reason,
            }));
        }
    }
    Ok(out)
}

pub fn cmd_digest(ctx: &AppCtx) -> Result<Value> {
    let last: i64 = std::fs::read_to_string(last_path(ctx)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let mut lines = Vec::new();
    let mut changed = Vec::new();
    let mut registered = 0usize;
    // The next digest starts after everything shown here, including entries this process
    // changed after it started (built-ins installed on its first run).
    let mut seen = ctx.now;
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        let since = e.status_since.unwrap_or(e.created_at);
        if since <= last {
            continue;
        }
        seen = seen.max(since);
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
    // Session start never touches the lake: the ledger's own outcome facets.
    let keys = crate::scope::transfer::ledger_outcome_keys(ctx)?;
    let transfers: Vec<Value> =
        crate::scope::transfer::suggest(ctx, &keys)?.into_iter().map(|s| serde_json::to_value(&s)).collect::<Result<_, _>>()?;
    for t in &transfers {
        lines.push(format!(
            "transfer suggestion: {} → {} (`/rematch transfer apply {} --branch {}`)",
            t.get("name").and_then(Value::as_str).unwrap_or("?"),
            t.get("to_branch").and_then(Value::as_str).unwrap_or("?"),
            t.get("lesson_id").and_then(Value::as_str).unwrap_or("?"),
            t.get("to_branch").and_then(Value::as_str).unwrap_or("?"),
        ));
    }
    // Informational: never applied by the digest (`policy domain set` is the only writer).
    let domain_proposals = crate::policy_cmd::domain_suggestions(ctx, &ctx.domain_policy()?)?;
    for d in &domain_proposals {
        lines.push(format!(
            "declare domain {} for {}: {}",
            d["domain"].as_str().unwrap_or("?"),
            d["remote"].as_str().unwrap_or("?"),
            d["command"].as_str().unwrap_or("?"),
        ));
    }
    // Ship candidates (`ship::ready`, read-only): each line once per `<id>:<scope>`; the
    // JSON always lists every current one.
    let ship_ready = crate::scope::ship::ready(ctx)?;
    let shown: BTreeSet<String> = std::fs::read_to_string(ship_shown_path(ctx))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    for r in ship_ready.iter().filter(|r| !shown.contains(&ship_key(r))) {
        lines.push(format!("ship {} {}: {} → {}", r.scope, r.name, r.detail, r.command));
    }
    if !approvals.is_empty() {
        lines.push(format!("{} approval(s) pending", approvals.len()));
    }
    // The last `profile sync` stopped at a rebase conflict (DESIGN §20): shown until one syncs.
    let profile_conflicts = rematch_core::profile::read_conflicts(&ctx.paths);
    if !profile_conflicts.is_empty() {
        lines.push(crate::profile_cmd::conflict_line(&rematch_core::profile::dir(&ctx.paths), &profile_conflicts));
    }
    std::fs::write(last_path(ctx), seen.to_string())?;
    // Only current candidates: one that stops being ready and returns is announced again.
    let keys: BTreeSet<String> = ship_ready.iter().map(ship_key).collect();
    std::fs::write(ship_shown_path(ctx), keys.into_iter().map(|k| k + "\n").collect::<String>())?;
    Ok(json!({
        "since": last,
        "lines": lines,
        "changed": changed,
        "approvals": approvals,
        "ship_ready": ship_ready,
        "review": review,
        "stale": stale,
        "transfers": transfers,
        "domain_proposals": domain_proposals,
        "profile_conflicts": profile_conflicts,
        "off": ctx.paths.is_off(),
    }))
}
