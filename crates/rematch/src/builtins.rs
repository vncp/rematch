//! Built-in entries: the context policies of D22/D23 shipped as ordinary hook entries
//! (SPEC §Isolation: "Context truncation and stuck detection … will enter as entries
//! under the same counters, not as always-on changes"; DESIGN §10, §16).
//!
//! Each is a provisional `kind: hook` entry in branch `shared` with repo glob `**` and no
//! exts/stems (it matches every session), origin `Import { source: "builtin" }` and
//! `redirect_class: None` (recurrence = a correction or preference redirect while it ran,
//! never a scope one; `counters::recurred`). Because they are data-only hooks, `library::stage`
//! compiles their modules at stage time so they activate automatically as scoped provisional
//! trials (`library::hook_active`) without a separate approval action. They are kept or retired
//! by their counters like any entry; a retired or disabled built-in is never re-staged
//! automatically (`rematch builtins reinstall` does it on request).
//!
//! `ensure` runs from `AppCtx::open_with` after the improver install and costs one file
//! read when nothing changed: `builtins.version` holds the hash of the shipped specs.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use rematch_core::ledger::{enum_str, sha256_hex, EntryFilter};
use rematch_core::library::{self, BUILTIN_SOURCE, SHARED_BRANCH, SKILL_MD};
use rematch_core::types::{Edit, Entry, Family, HookMode, HookRule, Kind, Origin, Predicate, Status};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};
use crate::jobs::LockFile;

#[derive(Subcommand, Debug, Clone)]
pub enum BuiltinsCmd {
    /// The shipped built-in entries with their current entry, status, approval and counters.
    Ls,
    /// Stage a fresh provisional version of built-ins that are not live (retired, dormant,
    /// stale or missing); live ones are left alone. Default: every built-in.
    Reinstall { names: Vec<String> },
}

pub struct Builtin {
    pub name: &'static str,
    pub decision: &'static str,
    pub trigger_line: &'static str,
    pub hook: HookRule,
    /// SKILL.md body after the trigger line (laws, pitfalls).
    pub laws: &'static str,
}

fn rule(mode: HookMode, message: &str, pass_condition: &str) -> HookRule {
    HookRule { mode, tool: "*".into(), ..HookRule::block("", "", "", message, pass_condition) }
}

/// The shipped built-ins (DESIGN §10 "Built-in entries").
pub fn specs() -> Vec<Builtin> {
    let mut retrieve = rule(
        HookMode::Retrieve,
        "rematch: units of the loaded skill packages that match this request.",
        "A request that shares no stem with a loaded package's trigger line, or whose loaded packages hold no unit sharing 2 distinct stems with it, passes unchanged; \
         units only ever come from entries this session loaded, and at most 6000 bytes are added.",
    );
    retrieve.budget_bytes = Some(6000);
    retrieve.min_overlap = Some(2);
    let mut truncate = rule(
        HookMode::Truncate,
        "rematch: old tool outputs trimmed in the outgoing request.",
        "Tool results at most 6 user turns old, and any result of at most 1200 characters, reach the model unchanged; \
         the session file always keeps the full output.",
    );
    truncate.max_age_turns = Some(6);
    truncate.keep_chars = Some(1200);
    let mut stuck = rule(
        HookMode::Stuck,
        "rematch stuck-detector: this exact call (same tool, same arguments) already ran 3 times in a row. \
         Change the approach: different arguments, a different tool, or ask the user.",
        "Every call that differs from the call immediately before it passes, and so do the first 3 repetitions of any call; \
         re-running a command after any other call (an edit, a read) passes.",
    );
    stuck.repeats = Some(3);
    let mut check = rule(
        HookMode::Check,
        "rematch: compiler/type/lint diagnostics and unedited caller references after file edits.",
        "A turn that edits no files, or a repo with no detected check, runs nothing and injects nothing; \
         a check is bounded to 30 seconds and 4096 bytes of injected context.",
    );
    check.timeout_secs = Some(30);
    check.budget_bytes = Some(4096);
    vec![
        Builtin {
            name: "context-unit-retrieval",
            decision: "D22",
            trigger_line: "Use when a loaded skill package has units that match the current request: the harness appends them to the request instead of waiting for the model to read them.",
            hook: retrieve,
            laws: "## Laws\n\
                   - Before each model request the rematch extension builds a query from the last user message and the paths of the last 10 tool results, and appends one message with the best-matching units (`units/`, `facts/`) of the skill packages this session loaded.\n\
                   - A unit is sent only when it shares at least 2 distinct stems with the query; highest overlap first, shorter first, at most 6000 bytes. Nothing is appended when no unit matches.\n\
                   - The appended message exists only in the outgoing request; the session history is never rewritten.\n\n\
                   ## Pitfalls\n\
                   - A retrieved unit is an excerpt: read the package router for the laws that govern it.\n",
        },
        Builtin {
            name: "tool-output-truncation",
            decision: "D23",
            trigger_line: "Use when a session runs long: tool outputs older than 6 user turns are trimmed to their first 1200 characters in the outgoing request.",
            hook: truncate,
            laws: "## Laws\n\
                   - Before each model request, every tool result older than 6 user turns (more than 6 user messages after it) keeps its first 1200 characters followed by the marker `…[rematch: truncated N chars, age K turns]`.\n\
                   - Only the outgoing request changes: the session file keeps every output in full.\n\n\
                   ## Pitfalls\n\
                   - When an old output matters again, re-run or re-read it instead of guessing from the trimmed head.\n",
        },
        Builtin {
            name: "stuck-detector",
            decision: "D23",
            trigger_line: "Use when the agent repeats itself: after 3 consecutive identical tool calls (same tool, same arguments), the next identical call is blocked.",
            hook: stuck,
            laws: "## Laws\n\
                   - A call identical to the 3 calls immediately before it (same tool name, same arguments hash) is blocked with a message asking for a different approach.\n\
                   - Any different call in between resets the count, so re-running a command after an edit passes.\n\n\
                   ## Pitfalls\n\
                   - When blocked, change the arguments, use a different tool, or ask the user; retrying the same call is blocked again.\n",
        },
        Builtin {
            name: "fast-check",
            decision: "D23",
            trigger_line: "Use when editing code in a repository with a compiler or linter: runs a scoped fast check after edits and injects diagnostics and unedited caller references.",
            hook: check,
            laws: "## Laws\n\
                   - After file-editing tool calls finish, the harness runs the repo's detected fast check (`rematch check`: Cargo, tsc, ruff, pyright, mypy, or Go) scoped to the edited files, plus `git grep` references for any function or type whose signature or name changed in `git diff`.\n\
                   - Before the next model request, at most one compact message (≤ 4096 bytes) is injected: compiler/linter errors and `help:` suggestions first, then caller references in unedited files (capped at 20 lines), or a one-line `✓ <check> clean in Xs` receipt.\n\
                   - Nothing runs and nothing is injected on turns that do not edit files.\n\n\
                   ## Pitfalls\n\
                   - When the injected check already reports `✓ <check> clean`, avoid re-running the same check manually unless you edited again or need full test execution.\n",
        },
    ]
}

fn skill_md(b: &Builtin) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{trigger}\n\nBuilt-in rematch entry ({decision}). It activates automatically as a provisional trial and is kept or retired by its counters like any entry.\n\n{laws}",
        name = b.name,
        description = serde_json::to_string(b.trigger_line).unwrap_or_default(),
        trigger = b.trigger_line,
        decision = b.decision,
        laws = b.laws,
    )
}

/// Hash of everything the built-ins ship; a change re-runs the install pass.
fn version() -> String {
    let all: Vec<Value> = specs()
        .iter()
        .map(|b| json!({"name": b.name, "hook": b.hook, "skill": skill_md(b), "trigger": b.trigger_line}))
        .collect();
    sha256_hex(serde_json::to_string(&all).unwrap_or_default().as_bytes())
}

/// Every built-in entry of a name, oldest first.
fn entries_named(ctx: &AppCtx, name: &str) -> Result<Vec<Entry>> {
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .filter(library::is_builtin)
        .collect())
}

fn is_live(e: &Entry) -> bool {
    matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned)
}

/// Stage `b` as a new provisional entry (after `previous`, when re-installing).
fn stage(ctx: &AppCtx, b: &Builtin, previous: Option<&Entry>) -> Result<Entry> {
    let mut e = new_entry(
        NewEntry {
            kind: Kind::Hook,
            name: b.name.into(),
            branch: SHARED_BRANCH.into(),
            status: Status::Provisional,
            family: Family(rematch_core::counters::UNKNOWN_FAMILY.into()),
            origin: Origin::Import { source: BUILTIN_SOURCE.into() },
            predicate: Predicate { repo_glob: "**".into(), ..Default::default() },
            trigger_line: b.trigger_line.into(),
            path: Default::default(),
        },
        ctx.now,
    )?;
    e.hook = Some(b.hook.clone());
    e.redirect_class = None;
    if let Some(p) = previous {
        e.version = p.version + 1;
        e.predecessor = Some(p.id.clone());
    }
    let files = [Edit::Write { path: SKILL_MD.into(), content: skill_md(b) }];
    library::stage(&ctx.ledger, &ctx.paths, &e, &files).with_context(|| format!("stage built-in {}", b.name))
}

/// Idempotent install: stages every built-in that never existed. A built-in that was
/// retired, declined or went dormant stays so. Cheap when `builtins.version` is current.
pub fn ensure(ctx: &AppCtx) -> Result<()> {
    let marker = ctx.paths.root.join("builtins.version");
    let v = version();
    if std::fs::read_to_string(&marker).is_ok_and(|s| s.trim() == v) {
        return Ok(());
    }
    let Some(_lock) = LockFile::acquire(ctx.paths.root.join(".builtins.lock"))? else {
        return Ok(()); // another process is installing
    };
    if std::fs::read_to_string(&marker).is_ok_and(|s| s.trim() == v) {
        return Ok(());
    }
    for b in specs() {
        if entries_named(ctx, b.name)?.is_empty() {
            stage(ctx, &b, None)?;
        }
    }
    std::fs::write(&marker, &v)?;
    Ok(())
}

fn describe(ctx: &AppCtx, b: &Builtin) -> Result<Value> {
    let mode = enum_str(&b.hook.mode);
    let Some(e) = entries_named(ctx, b.name)?.pop() else {
        return Ok(json!({"name": b.name, "mode": mode, "decision": b.decision, "entry": null}));
    };
    let c = ctx.ledger.counters(&e.id)?;
    Ok(json!({
        "name": b.name, "mode": mode, "decision": b.decision, "id": e.id, "status": e.status, "version": e.version,
        "approved": library::hook_active(&ctx.paths, &e),
        "counters": {"matched": c.matched, "recurrence": c.recurrence, "harm": c.harm},
        "path": e.path,
    }))
}

pub fn cmd_builtins(ctx: &AppCtx, cmd: BuiltinsCmd) -> Result<Value> {
    match cmd {
        BuiltinsCmd::Ls => {
            let items = specs().iter().map(|b| describe(ctx, b)).collect::<Result<Vec<_>>>()?;
            let lines: Vec<String> = items
                .iter()
                .map(|x| match x.get("status") {
                    Some(s) => format!(
                        "{} ({}): {}{}",
                        x["name"].as_str().unwrap_or("?"),
                        x["mode"].as_str().unwrap_or("?"),
                        s.as_str().unwrap_or("?"),
                        if x["approved"] == true { ", approved" } else { ", awaiting approval" },
                    ),
                    None => format!("{} ({}): not installed", x["name"].as_str().unwrap_or("?"), x["mode"].as_str().unwrap_or("?")),
                })
                .collect();
            Ok(json!({"builtins": items, "lines": lines}))
        }
        BuiltinsCmd::Reinstall { names } => {
            let all = specs();
            for n in &names {
                if !all.iter().any(|b| b.name == n) {
                    bail!("no built-in named {n:?} (one of {})", all.iter().map(|b| b.name).collect::<Vec<_>>().join(", "));
                }
            }
            let mut out = Vec::new();
            let mut lines = Vec::new();
            for b in all.iter().filter(|b| names.is_empty() || names.iter().any(|n| n == b.name)) {
                let existing = entries_named(ctx, b.name)?;
                if let Some(live) = existing.iter().find(|e| is_live(e)) {
                    lines.push(format!("{}: live ({}), left alone", b.name, enum_str(&live.status)));
                    out.push(json!({"name": b.name, "action": "live", "id": live.id}));
                    continue;
                }
                let e = stage(ctx, b, existing.last())?;
                lines.push(format!("{}: staged v{} (active provisional trial)", b.name, e.version));
                out.push(json!({"name": b.name, "action": "staged", "id": e.id, "version": e.version}));
            }
            Ok(json!({"builtins": out, "lines": lines}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rematch_core::ledger::Ledger;
    use rematch_core::lint::{lint_entry, LintConfig, LintContext};
    use rematch_core::paths::RematchPaths;

    fn ctx(dir: &std::path::Path) -> AppCtx {
        let paths = RematchPaths::with_roots(dir.join("rematch"), dir.join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: dir.to_path_buf(), text: false }
    }

    fn builtins(ctx: &AppCtx) -> Vec<Entry> {
        ctx.ledger.list_entries(&EntryFilter::default()).unwrap().into_iter().filter(library::is_builtin).collect()
    }

    #[test]
    fn ensure_is_idempotent_and_respects_retirement() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        ensure(&c).unwrap();
        let first = builtins(&c);
        assert_eq!(first.len(), 4);
        for e in &first {
            assert_eq!((e.kind, e.status, e.branch.as_str(), e.predicate.repo_glob.as_str()), (Kind::Hook, Status::Provisional, "shared", "**"));
            assert!(e.predicate.file_exts.is_empty() && e.predicate.stems.is_empty() && e.redirect_class.is_none());
            assert!(e.path.join("hook.json").is_file() && e.path.join("SKILL.md").is_file());
            let rep = lint_entry(&e.path, e, &LintConfig::default(), &LintContext::default()).unwrap();
            assert!(rep.ok, "{}: {:?}", e.name, rep.findings);
            assert!(library::hook_active(&c.paths, e), "{} should be active on stage", e.name);
        }
        // second run, and a run after the marker is gone: no duplicates
        ensure(&c).unwrap();
        std::fs::remove_file(c.paths.root.join("builtins.version")).unwrap();
        ensure(&c).unwrap();
        assert_eq!(builtins(&c).len(), 4);

        // a disabled built-in stays retired; reinstall stages v2 on request only
        let stuck = first.iter().find(|e| e.name == "stuck-detector").unwrap();
        library::disable(&c.ledger, &c.paths, &stuck.id, "test").unwrap();
        std::fs::remove_file(c.paths.root.join("builtins.version")).unwrap();
        ensure(&c).unwrap();
        assert_eq!(builtins(&c).iter().filter(|e| e.status == Status::Provisional).count(), 3);
        let r = cmd_builtins(&c, BuiltinsCmd::Reinstall { names: vec![] }).unwrap();
        let actions: Vec<(&str, &str)> =
            r["builtins"].as_array().unwrap().iter().map(|x| (x["name"].as_str().unwrap(), x["action"].as_str().unwrap())).collect();
        assert_eq!(
            actions,
            vec![
                ("context-unit-retrieval", "live"),
                ("tool-output-truncation", "live"),
                ("stuck-detector", "staged"),
                ("fast-check", "live"),
            ]
        );
        let v2 = builtins(&c).into_iter().find(|e| e.name == "stuck-detector" && e.status == Status::Provisional).unwrap();
        assert_eq!((v2.version, v2.predecessor.as_deref()), (2, Some(stuck.id.as_str())));
        assert!(cmd_builtins(&c, BuiltinsCmd::Reinstall { names: vec!["nope".into()] }).is_err());

        let ls = cmd_builtins(&c, BuiltinsCmd::Ls).unwrap();
        assert!(ls["builtins"].as_array().unwrap().iter().all(|x| x["approved"] == true && x["status"] == "provisional"), "{ls}");
    }

    #[test]
    fn builtins_activate_automatically_on_stage_and_are_listed_under_applied_automatically() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        ensure(&c).unwrap();
        let approvals = crate::digest::pending_approvals(&c).unwrap();
        assert!(approvals.is_empty(), "data-only built-ins need no approval action: {approvals:?}");

        let m = library::list_for_match(&c.ledger, &c.paths, None, t.path()).unwrap();
        assert_eq!(m.skill_dirs, vec![c.paths.staging.join("shared")]);
        assert_eq!(m.hooks.len(), 4, "{m:?}");
        assert_eq!(m.entries.len(), 4, "{m:?}");
        assert!(m.entries.iter().all(|e| e.predicate_hit));

        let digest = crate::digest::cmd_digest(&c).unwrap();
        assert!(digest["approvals"].as_array().unwrap().is_empty(), "{digest}");
        let auto = digest["applied_automatically"].as_array().unwrap();
        assert_eq!(auto.len(), 4, "{auto:?}");
        assert!(auto.iter().all(|a| a["why"] == "data-only" && a["undo"].as_str().unwrap().starts_with("rematch library disable ")));
    }
}
