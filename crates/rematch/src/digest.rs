//! `match --cwd` (load-time payload for the extension factory: entries, predicate hits, hook
//! modules, approved prompt fragments, personal instructions) and `digest --cwd` (session-start
//! lines, the pending action queue, capability-floor review, transfers). Skill delivery is
//! `skills sync` (`skills_cmd`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use rematch_core::features::{exts_of, stems_of_session};
use rematch_core::ledger::{enum_str, EntryFilter};
use rematch_core::domain;
use rematch_core::library;
use rematch_core::predicate;
use rematch_core::types::{Entry, Kind, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;

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

/// The digest's confirm items: every approvable action (`actions::pending`; entry parts,
/// personal instructions, exports, ship candidates, probe replays), diffs cut to
/// `actions::DIFF_BYTES`.
pub fn pending_approvals(ctx: &AppCtx) -> Result<Vec<Value>> {
    Ok(crate::actions::approvals(&crate::actions::pending(ctx)?))
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
    // The action queue is recomputed from the library every time (never from `digest.last`):
    // whatever waits for the user is shown until approved or declined.
    let actions = crate::actions::pending(ctx)?;
    let approvals = crate::actions::approvals(&actions);
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
    // Ship candidates are actions now (`ship-team:`/`fork:`); `ship_ready` stays as data.
    let ship_ready = crate::scope::ship::ready(ctx)?;
    lines.extend(actions.iter().map(crate::actions::Action::line));
    // Probe outcomes recorded since the last digest (`rematch status` lists the latest of each).
    let probes = crate::probes::statuses(ctx)?;
    lines.extend(probes.iter().filter(|p| p["at"].as_i64().is_some_and(|t| t > last)).filter_map(|p| p["line"].as_str().map(str::to_string)));
    if !actions.is_empty() {
        lines.push(format!(
            "{} action(s) pending, {} approvable (`rematch status`; `rematch library inspect <id>`)",
            actions.len(),
            approvals.len()
        ));
    }
    // The last `profile sync` stopped at a rebase conflict (DESIGN §20): shown until one syncs.
    let profile_conflicts = rematch_core::profile::read_conflicts(&ctx.paths);
    if !profile_conflicts.is_empty() {
        lines.push(crate::profile_cmd::conflict_line(&rematch_core::profile::dir(&ctx.paths), &profile_conflicts));
    }
    std::fs::write(last_path(ctx), seen.to_string())?;
    // The one-time ship notice file of earlier versions: superseded by the action queue.
    let _ = std::fs::remove_file(ctx.paths.root.join("digest.ship"));
    Ok(json!({
        "since": last,
        "lines": lines,
        "changed": changed,
        "approvals": approvals,
        "actions": actions.iter().map(crate::actions::Action::compact).collect::<Vec<_>>(),
        "applied": crate::actions::applied(ctx)?.iter().map(crate::actions::Action::compact).collect::<Vec<_>>(),
        "trials": crate::actions::trials(ctx)?,
        "instructions": rematch_core::instructions::exposure(&ctx.ledger, &ctx.paths)?,
        "ship_ready": ship_ready,
        "probes": probes,
        "review": review,
        "stale": stale,
        "transfers": transfers,
        "domain_proposals": domain_proposals,
        "profile_conflicts": profile_conflicts,
        "off": ctx.paths.is_off(),
    }))
}
