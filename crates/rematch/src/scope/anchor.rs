//! Anchor replay (SPEC "Rollback and anchors"; research D17, HCL regression budget b=1).
//!
//! Before an entry replaces a kept one, the kept entry's originating task is replayed ONCE
//! in batch mode with the replacing entry in its place, when that task has a verifier;
//! otherwise the replacing entry's own canary (its counters) is the anchor. There is no
//! separate anchor suite: one rollout, one verdict.
//!
//! An entry replaces a kept one when its `sibling_of` names a kept entry, or when keeping it
//! takes the kept slot (`<skills>/<name>/`, agents `kept-agents/<name>/`) a kept entry holds
//! (`library::set_status` would supersede that entry). Three call sites gate a keep:
//! `reflect::apply_decision` (counters decided Keep), the batch worker's next-run rule
//! (`Cycle::resolve_pending`) and `transfer::promote_shared` (a shared copy taking a
//! branch-kept slot). Pass ⇒ the keep proceeds and the replacing entry gets a receipt
//! `Certificate{verifier}`; Fail ⇒ it is retired with `anchor replay failed: …` and the kept
//! entry stays. `rematch anchor replay <entry>` runs the same replay by hand and changes
//! nothing but the blob store.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use rematch_core::features::head;
use rematch_core::ledger::EntryFilter;
use rematch_core::library::{branch_for, SHARED_BRANCH};
use rematch_core::lint::{lint_entry, LintConfig, LintContext};
use rematch_core::types::{Activation, Certificate, Entry, Origin, Paired, Receipt, Sha256Hex, Status, Verdict};
use serde::Serialize;
use serde_json::{json, Value};

use super::{resolve_entry, short_id};
use crate::app::AppCtx;
use crate::batch::rollout::{run_rollout, workspace, RolloutSummary};
use crate::batch::substrate::{read_entry_files, BaseLibrary, CandidateTree, OmpSubstrate, Substrate, TreeEntry};
use crate::batch::tasks::{is_git_repo, list_tasks, load_task, task_runs_dir, Task};
use crate::batch::verifier::{detail_of, is_pass, parse_spec};
use crate::config::{resolve_role, ROLE_SOLVER};
use crate::jobs::LockFile;
use crate::omp;

/// Chars of the verifier detail kept in a retired entry's `status_note`.
const NOTE_DETAIL_CHARS: usize = 300;
/// Prefix of the `status_note` of an entry retired by a failed anchor replay.
pub const FAILED_NOTE: &str = "anchor replay failed: ";

/// What one anchor replay produced (also stored as a ledger blob).
#[derive(Debug, Clone, Serialize)]
pub struct AnchorReceipt {
    pub task: String,
    /// Verifier spec the replay ran (the task's own, else the one its last run used).
    pub verifier: String,
    pub verdict: Verdict,
    pub rollout: RolloutSummary,
    /// sha256 of the replay record in the ledger's blob store.
    pub blob: Sha256Hex,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AnchorOutcome {
    /// One rollout of the kept entry's originating task with the replacing entry in place.
    Replayed { pass: bool, receipt: Box<AnchorReceipt> },
    /// No originating task with a verifier: the replacing entry's canary is the anchor.
    NoVerifier,
}

#[derive(Debug, Clone, clap::Args)]
pub struct AnchorArgs {
    #[command(subcommand)]
    pub cmd: AnchorCmd,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum AnchorCmd {
    /// Replay the originating task of the kept entry <entry> would replace, with <entry> in
    /// its place; reports the verdict and changes no status.
    Replay { entry: String },
}

pub fn cmd_anchor(ctx: &AppCtx, args: AnchorArgs) -> Result<Value> {
    match args.cmd {
        AnchorCmd::Replay { entry } => {
            let e = resolve_entry(ctx, &entry)?;
            let kept = replaced_kept(ctx, &e)?.ok_or_else(|| {
                anyhow!(
                    "entry {} ({}) replaces no kept entry: its sibling_of is not kept and no kept entry holds its slot",
                    e.id,
                    e.name
                )
            })?;
            let outcome = replay_before_replace(ctx, &e, &kept)?;
            let line = match &outcome {
                AnchorOutcome::Replayed { pass, receipt } => format!(
                    "anchor replay of task {} for {} (replacing {}): {} ({})",
                    receipt.task,
                    e.name,
                    kept.id,
                    if *pass { "pass" } else { "fail" },
                    receipt.verifier
                ),
                AnchorOutcome::NoVerifier => format!(
                    "{} replacing {}: no originating task with a verifier; its canary is the anchor",
                    e.name, kept.id
                ),
            };
            Ok(json!({
                "entry": e.id,
                "name": e.name,
                "kept": kept.id,
                "anchor": outcome,
                "staged": false,
                "lines": [line],
            }))
        }
    }
}

/// The kept entry `e` replaces when it becomes kept: the entry its `sibling_of` names when
/// that one is kept, else the kept entry holding the slot `library::set_status` gives `e`
/// (rematch-owned entries only; a foreign entry's keep moves no files).
pub fn replaced_kept(ctx: &AppCtx, e: &Entry) -> Result<Option<Entry>> {
    if let Some(id) = &e.sibling_of {
        if let Some(k) = ctx.ledger.get_entry(id)? {
            if k.status == Status::Kept && k.id != e.id {
                return Ok(Some(k));
            }
        }
    }
    if crate::reflect::is_foreign(ctx, e) {
        return Ok(None);
    }
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { status: Some(Status::Kept), name: Some(e.name.clone()), ..Default::default() })?
        .into_iter()
        .find(|k| k.id != e.id && rematch_core::library::is_kept_slot(&ctx.paths, e, &k.path)))
}

/// Replay before `sibling` replaces `kept` (omp from `$OMP_BIN`/PATH). Takes the anchor
/// task's run lock; a task already running is an error (the caller decides again later).
pub fn replay_before_replace(ctx: &AppCtx, sibling: &Entry, kept: &Entry) -> Result<AnchorOutcome> {
    replay(ctx, sibling, kept, &omp::omp_bin(), None)
}

/// [`replay_before_replace`] from inside a batch run of `held_task` (whose lock the caller
/// holds) with that run's omp binary.
pub fn replay_in_run(ctx: &AppCtx, sibling: &Entry, kept: &Entry, omp_bin: &Path, held_task: &str) -> Result<AnchorOutcome> {
    replay(ctx, sibling, kept, omp_bin, Some(held_task))
}

fn replay(ctx: &AppCtx, sibling: &Entry, kept: &Entry, omp_bin: &Path, held_task: Option<&str>) -> Result<AnchorOutcome> {
    let Some((task, spec)) = anchor_task(ctx, sibling, kept)? else {
        return Ok(AnchorOutcome::NoVerifier);
    };
    let verifier = parse_spec(&spec).with_context(|| format!("verifier of task {}", task.id))?;
    if !sibling.path.is_dir() {
        bail!("entry dir missing: {}", sibling.path.display());
    }
    let runs = task_runs_dir(ctx, &task.id);
    std::fs::create_dir_all(&runs)?;
    let _lock = if held_task == Some(task.id.as_str()) {
        None
    } else {
        Some(
            LockFile::acquire(runs.join(".lock"))?
                .ok_or_else(|| anyhow!("another run of task {} is in progress; anchor replay deferred", task.id))?,
        )
    };
    let solver = resolve_role(&ctx.paths, &task.repo, ROLE_SOLVER)?.model;
    let sub = OmpSubstrate { repo: task.repo.clone(), solver };
    let base = BaseLibrary::load(ctx, &task.repo)?.without(&[kept.id.clone(), sibling.id.clone()]);
    let candidate = TreeEntry { name: sibling.name.clone(), id: Some(sibling.id.clone()), files: read_entry_files(&sibling.path)? };
    let tree = CandidateTree { entries: vec![candidate], base };
    let seed = task.first_seed();
    let label = format!("anchor-{}", short_id(&sibling.id));
    let dest = workspace(ctx, &task, seed, &label);
    let r = run_rollout(ctx, &sub, omp_bin, &tree, &task, seed, &label, Some(verifier.as_ref()));
    let cleanup = match &r {
        Ok(r) => sub.cleanup(&r.install),
        Err(_) => remove_partial(&dest, &task),
    };
    if let Some(seed_dir) = dest.parent() {
        // Only when empty: a batch run of the same task may own other workspaces there.
        let _ = std::fs::remove_dir(seed_dir);
    }
    let r = r?;
    cleanup?;
    let verdict = r.verdict.clone().ok_or_else(|| anyhow!("anchor rollout of task {} has no verdict", task.id))?;
    let pass = is_pass(&verdict);
    let rollout = r.summary();
    let record = json!({
        "kind": "anchor_replay",
        "sibling": sibling.id,
        "kept": kept.id,
        "task": task.id,
        "verifier": spec,
        "verdict": verdict,
        "rollout": rollout,
        "at": ctx.now,
    });
    let blob = ctx.ledger.put_blob(serde_json::to_string(&record)?.as_bytes())?;
    let receipt = Box::new(AnchorReceipt { task: task.id, verifier: spec, verdict, rollout, blob });
    Ok(AnchorOutcome::Replayed { pass, receipt })
}

/// A rollout that failed before it produced an `Install`: remove what materialize left.
fn remove_partial(dest: &Path, task: &Task) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    if is_git_repo(&task.repo) {
        let _ = super::git(&task.repo, &["worktree", "prune"]);
    }
    Ok(())
}

/// The anchor task and its verifier spec. Candidates in order: the kept entry's
/// `Origin::Task`, the replacing entry's, then tasks in the kept entry's branch whose last
/// receipt lists the kept entry (staged by it, merged into, decided by its next-run rule, or
/// loaded by its reference rollout). The first one with a verifier wins.
pub fn anchor_task(ctx: &AppCtx, sibling: &Entry, kept: &Entry) -> Result<Option<(Task, String)>> {
    let mut ids: Vec<String> = Vec::new();
    for e in [kept, sibling] {
        if let Origin::Task { task_id } = &e.origin {
            if !ids.contains(task_id) {
                ids.push(task_id.clone());
            }
        }
    }
    for t in list_tasks(ctx)? {
        if !ids.contains(&t.id) && in_branch(&t, &kept.branch) && last_receipt(ctx, &t.id).is_some_and(|r| receipt_lists(&r, &kept.id)) {
            ids.push(t.id);
        }
    }
    for id in ids {
        // A task deleted since it staged the entry is no anchor.
        let Ok(task) = load_task(ctx, &id) else { continue };
        if let Some(spec) = verifier_spec(ctx, &task) {
            return Ok(Some((task, spec)));
        }
    }
    Ok(None)
}

fn in_branch(t: &Task, branch: &str) -> bool {
    branch == SHARED_BRANCH || branch_for(is_git_repo(&t.repo).then_some(t.repo.as_path()), &t.repo) == branch
}

fn last_receipt(ctx: &AppCtx, task_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(task_runs_dir(ctx, task_id).join("receipt.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn receipt_lists(r: &Value, id: &str) -> bool {
    let listed = |key: &str| {
        r[key].as_array().is_some_and(|a| a.iter().any(|x| x["id"] == id || x["merged_into"] == id))
    };
    let loaded_by_reference = r["rollouts"].as_array().is_some_and(|a| {
        a.iter().any(|x| x["label"] == "reference" && x["loaded"].as_array().is_some_and(|l| l.iter().any(|v| v == id)))
    });
    listed("staged") || listed("next_run") || loaded_by_reference
}

/// The task's own verifier, else the one its last `run` used (`receipt.json` job spec),
/// else the repository's default (`check`, then `tests`).
fn verifier_spec(ctx: &AppCtx, task: &Task) -> Option<String> {
    let own = task.verifier.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    own.or_else(|| {
        last_receipt(ctx, &task.id)?["job_spec"]["verifier"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
    .or_else(|| crate::batch::verifier::default_spec_for(&task.repo).map(|(v, _)| v))
}

/// `status_note` of an entry retired by a failed anchor replay.
pub fn failure_note(r: &AnchorReceipt) -> String {
    let detail = head(detail_of(&r.verdict), NOTE_DETAIL_CHARS).split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{FAILED_NOTE}task {} ({}): {detail}", r.task, r.verifier)
}

/// Pass ⇒ the entry that replaced the kept one carries the anchor verdict in a receipt
/// (`Certificate{verifier}`; `canary` = its counters when they kept it).
pub fn certify(ctx: &AppCtx, e: &Entry, r: &AnchorReceipt, canary: Option<Paired>) -> Result<Sha256Hex> {
    let cfg = LintConfig::load(&ctx.improver_file("lint.toml"))?;
    let lint = lint_entry(&e.path, e, &cfg, &LintContext::default())?;
    let cert = Certificate { lint, canary, verifier: Some(r.verdict.clone()) };
    let sha = ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
    ctx.ledger.insert_receipt(&Receipt { certificate: sha.clone(), entry: e.id.clone(), activation: Activation::Personal })?;
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_receipt_listing() {
        let r = json!({
            "staged": [{"id": "a"}, {"name": "x", "merged_into": "b"}],
            "next_run": [{"id": "c", "status": "kept"}],
            "rollouts": [
                {"label": "reference", "loaded": ["d"]},
                {"label": "domain-1", "loaded": ["e"]},
            ],
        });
        for id in ["a", "b", "c", "d"] {
            assert!(receipt_lists(&r, id), "{id}");
        }
        assert!(!receipt_lists(&r, "e"), "a candidate rollout's harness is not the task's");
        assert!(!receipt_lists(&json!({}), "a"));
    }
}
