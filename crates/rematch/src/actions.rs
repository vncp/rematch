//! Durable action inventory (DESIGN §16): every staged artifact that waits for the user —
//! personal instructions, edited personal-instruction blocks, scripts, hooks, provisional
//! agents, exports, team/fork ship candidates and probe replays — as one typed list that
//! `digest`, `status`, `library ls`, `library inspect` and every host UI render. It is computed
//! from the library on every call, never from the digest cursor: an action stays until the user
//! approves or declines it or its artifact goes. Approve and decline are user commands
//! (`library approve|decline <id>`); the model-facing tool refuses them.
//!
//! Automatic trials (provisional skills, rules, facts) are not actions: they are observable
//! through [`trials`] and `library ls` with their history, and leave with `library disable`.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use rematch_core::instructions;
use rematch_core::ledger::{enum_str, now_ms, sha256_hex, EntryFilter};
use rematch_core::library::{self, Approval, AGENTS_FRAGMENT, AGENT_MD};
use rematch_core::types::{Activation, Entry, Kind, Receipt, Status};
use serde::Serialize;
use serde_json::{json, Value};

use crate::app::AppCtx;

/// Max bytes of a diff in `digest`/`status`/`library ls` (`library inspect` shows it whole).
pub const DIFF_BYTES: usize = 4096;
/// What an always-on change means for sessions already running.
pub const RELOAD: &str = "applies to new sessions; a running session keeps the text it was already sent";
/// The eight harness surfaces (DESIGN §16).
pub const SURFACES: [&str; 8] = ["system_prompt", "rules", "memory", "tool_guidance", "subagents", "skills", "scripts", "hooks"];
/// Prefix of a personal-instructions action id (`instructions:<entry id>`).
pub const INSTRUCTIONS: &str = "instructions:";
/// Prefix of an edited personal-instructions block (`instructions-conflict:<name>`).
pub const INSTRUCTIONS_CONFLICT: &str = "instructions-conflict:";
/// Prefix of an approved personal-instructions block, applied or over the budget
/// (`instructions-applied:<name>`): inspectable and withdrawable (decline), never pending.
pub const INSTRUCTIONS_APPLIED: &str = "instructions-applied:";
/// The personal `AGENTS.md` is not what the approvals, the profile, the budget and on/off say
/// (a failed write, an unreadable file, no sync since): approve writes it now.
pub const INSTRUCTIONS_SYNC: &str = "instructions-sync";
/// Prefix of an approved runtime fragment the other hosts cannot read
/// (`fragment-unreadable:<entry id>`): blocked; decline withdraws the runtime approval.
pub const FRAGMENT_UNREADABLE: &str = "fragment-unreadable:";
/// Prefix of a team ship candidate (`ship-team:<entry id>`).
pub const SHIP_TEAM: &str = "ship-team:";
/// Prefix of a fork ship candidate (`fork:<entry id>`).
pub const FORK: &str = "fork:";
/// Prefix of a probe replay authorization (`probe:<id>`, `crate::probes`).
pub const PROBE: &str = "probe:";
/// Prefix of a home export (`export:<entry id>`).
pub const EXPORT: &str = "export:";
/// `what` of the receipt blob recording a declined ship candidate (`scope`: team|fork).
const DECLINE_SHIP: &str = "decline-ship";

/// `id` names an entry (its scripts/hook/agent parts), not a prefixed action.
pub fn entry_id(id: &str) -> bool {
    id != INSTRUCTIONS_SYNC
        && ![INSTRUCTIONS, INSTRUCTIONS_CONFLICT, INSTRUCTIONS_APPLIED, FRAGMENT_UNREADABLE, SHIP_TEAM, FORK, PROBE, EXPORT]
            .iter()
            .any(|p| id.starts_with(p))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionEntry {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub status: Status,
    pub content_sha256: Option<String>,
}

impl ActionEntry {
    pub fn of(e: &Entry) -> ActionEntry {
        ActionEntry { id: e.id.clone(), name: e.name.clone(), version: e.version, status: e.status, content_sha256: e.content_sha256.clone() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Commands {
    pub approve: Option<String>,
    pub decline: Option<String>,
    pub inspect: Option<String>,
    /// `rematch` argv of the approve command (hosts run this, never a parsed string).
    pub approve_argv: Option<Vec<String>>,
    pub decline_argv: Option<Vec<String>>,
}

impl Commands {
    /// `library approve|decline|inspect <id>` (approve with `--sha <sha>` when given). Every
    /// action of [`pending`] gets [`Commands::bound`] commands once finalized.
    pub fn for_id(id: &str, sha: Option<&str>) -> Commands {
        let mut approve = vec!["library".to_string(), "approve".into(), id.into()];
        if let Some(s) = sha {
            approve.extend(["--sha".to_string(), s.to_string()]);
        }
        let decline = vec!["library".to_string(), "decline".into(), id.into()];
        Commands {
            approve: Some(format!("rematch {}", approve.join(" "))),
            decline: Some(format!("rematch {}", decline.join(" "))),
            inspect: Some(format!("rematch library inspect {id}")),
            approve_argv: Some(approve),
            decline_argv: Some(decline),
        }
    }

    /// Approve and decline bound to the reviewed snapshot (`--sha <review_sha256>`); each only
    /// when the action offers it.
    pub fn bound(id: &str, review: &str, approve: bool, decline: bool) -> Commands {
        let argv = |verb: &str| vec!["library".to_string(), verb.into(), id.into(), "--sha".into(), review.into()];
        let (a, d) = (approve.then(|| argv("approve")), decline.then(|| argv("decline")));
        Commands {
            approve: a.as_ref().map(|v| format!("rematch {}", v.join(" "))),
            decline: d.as_ref().map(|v| format!("rematch {}", v.join(" "))),
            inspect: Some(format!("rematch library inspect {id}")),
            approve_argv: a,
            decline_argv: d,
        }
    }

    /// No approve (blocked): decline and inspect stay.
    pub fn without_approve(mut self) -> Commands {
        self.approve = None;
        self.approve_argv = None;
        self
    }
}

/// One place an approval makes the text reach.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reach {
    /// `omp`, or the hosts sharing the runtime insertion (`claude-code|pi|opencode`).
    pub host: String,
    pub target: String,
    pub scope: String,
    /// `false` when an earlier approval already grants this reach for these exact bytes.
    pub new: bool,
}

/// One thing waiting for the user. `id` is the argument of `library approve|decline|inspect`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub id: String,
    /// `instructions` | `instructions_conflict` | parts joined by `+` (`scripts`, `hook`,
    /// `agent`) | `export` | `ship` | `fork` | `probe`.
    pub kind: String,
    /// One of [`SURFACES`].
    pub surface: String,
    pub name: String,
    pub entry: Option<ActionEntry>,
    /// The exact content hash the decision binds (fragment, module, fork file, replay spec).
    pub sha256: Option<String>,
    /// sha256 of the reviewed snapshot — id, kind, surface, entry, `sha256`, target, scope,
    /// effect, state, reaches, blocked reason, reason, summary and the whole diff. `--sha` of
    /// every approve and decline must equal it (all 64 hex digits), re-computed when the command
    /// runs.
    pub review_sha256: String,
    /// What the approval writes or runs: a path, `<repo>:<file>`, a task.
    pub target: String,
    /// `personal` | `machine` | `team` | `fork` | `home_export` | `probe`.
    pub scope: String,
    /// `always_on` | `executable` | `runtime_hook` | `subagent` | `external_branch` | `copy` | `replay`.
    pub effect: String,
    /// `pending` | `blocked` | `conflict` | `applied` (withdrawable, not waiting).
    pub state: String,
    pub reason: String,
    pub summary: String,
    pub diff: String,
    pub requires_approval: bool,
    /// Why it cannot be approved as is, with the command that fixes it.
    pub blocked: Option<String>,
    pub commands: Commands,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reaches: Vec<Reach>,
    pub reload: Option<String>,
    pub since: Option<i64>,
}

impl Action {
    pub fn new(id: &str, kind: &str, surface: &str, name: &str) -> Action {
        Action {
            id: id.into(),
            kind: kind.into(),
            surface: surface.into(),
            name: name.into(),
            entry: None,
            sha256: None,
            review_sha256: String::new(),
            target: String::new(),
            scope: String::new(),
            effect: String::new(),
            state: "pending".into(),
            reason: String::new(),
            summary: String::new(),
            diff: String::new(),
            requires_approval: true,
            blocked: None,
            commands: Commands::for_id(id, None),
            reaches: Vec::new(),
            reload: None,
            since: None,
        }
    }

    /// Mark blocked: no approve command.
    pub fn block(&mut self, why: impl Into<String>) {
        self.blocked = Some(why.into());
        self.state = "blocked".into();
        self.commands = std::mem::take(&mut self.commands).without_approve();
    }

    /// The digest's confirm item: pending, approvable.
    pub fn approvable(&self) -> bool {
        self.state == "pending" && self.requires_approval && self.blocked.is_none() && self.commands.approve_argv.is_some()
    }

    /// Hash the reviewed snapshot and bind the commands to it.
    pub fn finalize(&mut self) {
        let snapshot = json!({
            "id": self.id, "kind": self.kind, "surface": self.surface, "entry": self.entry, "sha256": self.sha256,
            "target": self.target, "scope": self.scope, "effect": self.effect, "state": self.state,
            "reaches": self.reaches, "blocked": self.blocked, "reason": self.reason, "summary": self.summary, "diff": self.diff,
        });
        self.review_sha256 = sha256_hex(snapshot.to_string().as_bytes());
        let approve = self.commands.approve_argv.is_some();
        let decline = self.commands.decline_argv.is_some();
        self.commands = Commands::bound(&self.id, &self.review_sha256, approve, decline);
    }

    /// JSON with the diff cut to [`DIFF_BYTES`].
    pub fn compact(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        v["diff"] = json!(truncate(&self.diff, DIFF_BYTES));
        v
    }

    /// One status line. It names the review command (`library inspect <id>`, which shows the
    /// exact diff and the approve/decline commands bound to it), never a command with the
    /// snapshot hash: that hash binds machine-local targets, and a decision follows a review.
    pub fn line(&self) -> String {
        let inspect = self.commands.inspect.as_deref().unwrap_or("");
        match (self.state.as_str(), &self.blocked) {
            (_, Some(why)) => format!("blocked {}: {} — {why}", self.kind, self.summary),
            ("conflict", None) => format!("conflict: {} — `{inspect}`", self.summary),
            ("applied", None) => format!("applied: {} — `{inspect}` to withdraw", self.summary),
            _ => format!("approval pending: {} — `{inspect}` to review, then approve or decline", self.summary),
        }
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[+{} bytes]", &s[..end], s.len() - end)
}

/// The surface an entry of this kind changes.
pub fn surface_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Agent => "subagents",
        Kind::Hook => "hooks",
        Kind::Rule => "rules",
        Kind::Fact | Kind::Lesson => "memory",
        Kind::Script => "scripts",
        Kind::Skill => "skills",
    }
}

fn reason_of(e: &Entry) -> String {
    e.manifest
        .as_ref()
        .map(|m| format!("{} Predicted fix: {}", m.rationale, m.predicted_fix).trim().to_string())
        .unwrap_or_else(|| e.trigger_line.clone())
}

fn sha12(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// Line diff (`-`/`+`/` ` prefixes) of `old` → `new` by longest common subsequence; above
/// 4 M line pairs it falls back to all-removed/all-added.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        a.iter().for_each(|l| out.push_str(&format!("-{l}\n")));
        b.iter().for_each(|l| out.push_str(&format!("+{l}\n")));
        return out;
    }
    let (n, m) = (a.len(), b.len());
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if a[i] == b[j] { t[i + 1][j + 1] + 1 } else { t[i + 1][j].max(t[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push_str(&format!(" {}\n", a[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || t[i][j + 1] > t[i + 1][j]) {
            out.push_str(&format!("+{}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{}\n", a[i]));
            i += 1;
        }
    }
    out
}

fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
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
    out.sort();
    out
}

/// The entry's parts awaiting the one tap under its own id: [`library::pending_parts`] without
/// the prompt part, which is the separate `instructions:<id>` action (declining text never
/// retires the entry).
pub fn entry_parts(ctx: &AppCtx, e: &Entry) -> Vec<Approval> {
    library::pending_parts(&ctx.paths, e).into_iter().filter(|p| *p != Approval::Prompt).collect()
}

// ---------------------------------------------------------------------------------------------
// Inventory

/// Every action, diffs whole and commands bound to their reviewed snapshot, in a stable order:
/// entry parts (by status), personal instructions (pending, over the budget, the file out of
/// sync), their conflicts, exports, ship candidates, probe replays.
pub fn pending(ctx: &AppCtx) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    entry_actions(ctx, &mut out)?;
    instruction_actions(ctx, &mut out)?;
    export_actions(ctx, &mut out)?;
    ship_actions(ctx, &mut out)?;
    out.extend(crate::probes::pending_actions(ctx)?);
    out.iter_mut().for_each(Action::finalize);
    Ok(out)
}

/// Approvable actions as the digest's `approvals` items (diffs cut).
pub fn approvals(actions: &[Action]) -> Vec<Value> {
    actions.iter().filter(|a| a.approvable()).map(Action::compact).collect()
}

/// The personal-instruction blocks omp loads now (`instructions-applied:<name>`, state
/// `applied`): not waiting for anything, inspectable and withdrawable like a pending action.
pub fn applied(ctx: &AppCtx) -> Result<Vec<Action>> {
    let ex = instructions::exposure(&ctx.ledger, &ctx.paths)?;
    let recs = instructions::records(&ctx.ledger, &ctx.paths)?;
    let set = instructions::desired(&ctx.ledger, &ctx.paths, &recs, false, None)?;
    let shown = ex.target.display().to_string();
    let profile = rematch_core::profile::present(&ctx.paths);
    let mut out = Vec::new();
    for x in ex.exposed.iter().filter(|x| x.source != "stale") {
        let Some(w) = set.blocks.get(&x.name).filter(|w| w.sha == x.sha) else { continue };
        let mut a = text_action(ctx, w, &shown, profile, "applied");
        a.summary = format!("personal instructions {} in {shown} ({} B, fragment {}, {})", w.name, x.bytes, sha12(&w.sha), x.source);
        a.finalize();
        out.push(a);
    }
    Ok(out)
}

/// One line naming the applied blocks and how to inspect or withdraw each (`None`: none).
pub fn applied_line(applied: &[Action]) -> Option<String> {
    let first = applied.first()?;
    Some(format!(
        "personal instructions applied in {}: {} (`rematch library inspect {INSTRUCTIONS_APPLIED}<name>` shows one and how to withdraw it)",
        first.target,
        applied.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
    ))
}

/// One artifact or decision rematch applied without asking the user (`applied_automatically`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutoApplied {
    pub id: String,
    pub name: String,
    pub what: String,
    pub where_target: String,
    /// `data-only` | `read-only` | `rule` | `comparative_support` | `probe_regressed`.
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
    pub undo: String,
}

impl AutoApplied {
    pub fn line(&self) -> String {
        let why = match &self.rule {
            Some(r) => format!("{} ({r})", self.why),
            None => self.why.clone(),
        };
        format!("applied automatically: {} in {} ({why}) — undo: `{}`", self.what, self.where_target, self.undo)
    }
}

fn undo_for_entry(e: &Entry) -> String {
    if matches!(e.status, Status::Kept | Status::Pinned) && e.predecessor.is_some() {
        format!("rematch library rollback {}", e.name)
    } else {
        format!("rematch library disable {}", e.id)
    }
}

/// Everything rematch applied automatically without a user prompt: data-only hooks, read-only
/// provisional subagents, surfaces matched by a standing `policy.json` `auto` rule, and automatic
/// probe keep/retire decisions. Empty while rematch is off.
pub fn applied_automatically(ctx: &AppCtx) -> Result<Vec<AutoApplied>> {
    if ctx.paths.is_off() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            if e.kind == Kind::Lesson {
                continue;
            }
            let undo = undo_for_entry(&e);
            if e.kind == Kind::Hook && library::is_data_only_hook(&e) {
                let mode = e.hook.as_ref().map(|h| enum_str(&h.mode)).unwrap_or_else(|| "hook".into());
                out.push(AutoApplied {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    what: format!("hook {} ({mode})", e.name),
                    where_target: format!("<agent_dir>/extensions/rematch-hooks/{}.ts", e.id),
                    why: "data-only".into(),
                    rule: None,
                    observed: None,
                    undo: undo.clone(),
                });
            }
            if e.kind == Kind::Agent && e.status == Status::Provisional && library::agent_approved(&e) {
                let rec = library::latest_approval_record(&ctx.ledger, &e.id, Approval::Agent);
                let approver = rec.as_ref().and_then(|r| r.approver.as_deref());
                let rule = approver.and_then(|a| a.strip_prefix("rule:")).map(str::to_string);
                if approver == Some("read-only") || rule.is_some() || (approver.is_none() && library::agent_is_read_only(&e)) {
                    out.push(AutoApplied {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        what: format!("agent {}", e.name),
                        where_target: library::agent_link(&ctx.paths, &e.name).display().to_string(),
                        why: if rule.is_some() { "rule".into() } else { "read-only".into() },
                        rule,
                        observed: None,
                        undo: undo.clone(),
                    });
                }
            }
            if !e.approved_scripts.is_empty() && library::unapproved_scripts(&e).is_empty() {
                if let Some(rec) = library::latest_approval_record(&ctx.ledger, &e.id, Approval::Scripts) {
                    if let Some(approver) = rec.approver.as_deref() {
                        let Some(rule) = approver.strip_prefix("rule:").map(str::to_string) else { continue };
                        out.push(AutoApplied {
                            id: e.id.clone(),
                            name: e.name.clone(),
                            what: format!("scripts of {} ({})", e.name, e.approved_scripts.join(", ")),
                            where_target: e.path.join("scripts").display().to_string(),
                            why: "rule".into(),
                            rule: Some(rule),
                            observed: rec.observed,
                            undo: undo.clone(),
                        });
                    }
                }
            }
            if matches!(e.status, Status::Kept | Status::Pinned)
                && e.retention.as_ref().is_some_and(|r| r.reason == rematch_core::types::RetentionReason::ComparativeSupport)
            {
                out.push(AutoApplied {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    what: format!("kept {} {}", enum_str(&e.kind), e.name),
                    where_target: e.path.display().to_string(),
                    why: "comparative_support".into(),
                    rule: None,
                    observed: None,
                    undo: undo.clone(),
                });
            }
        }
    }
    for e in ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Retired), ..Default::default() })? {
        let regressed = crate::probes::record_of(ctx, &e.id).is_some_and(|r| r.status == crate::probes::ProbeStatus::Regresses)
            || e.status_note.as_deref().is_some_and(|n| n.starts_with("bounded comparison regressed"));
        if regressed {
            out.push(AutoApplied {
                id: e.id.clone(),
                name: e.name.clone(),
                what: format!("retired {} {}", enum_str(&e.kind), e.name),
                where_target: e.path.display().to_string(),
                why: "probe_regressed".into(),
                rule: None,
                observed: None,
                undo: undo_for_entry(&e),
            });
        }
    }
    let ex = instructions::exposure(&ctx.ledger, &ctx.paths)?;
    let recs = instructions::records(&ctx.ledger, &ctx.paths)?;
    let set = instructions::desired(&ctx.ledger, &ctx.paths, &recs, false, None)?;
    let shown = ex.target.display().to_string();
    for x in ex.exposed.iter().filter(|x| x.source != "stale") {
        let Some(w) = set.blocks.get(&x.name).filter(|w| w.sha == x.sha) else { continue };
        let Some(approver) = w.approver.as_deref() else { continue };
        let rule = approver.strip_prefix("rule:").map(str::to_string);
        out.push(AutoApplied {
            id: format!("{INSTRUCTIONS_APPLIED}{}", w.name),
            name: w.name.clone(),
            what: format!("instructions {}", w.name),
            where_target: shown.clone(),
            why: if rule.is_some() { "rule".into() } else { approver.into() },
            rule,
            observed: None,
            undo: w
                .entry
                .as_deref()
                .map(|id| format!("rematch library disable {id}"))
                .unwrap_or_else(|| format!("rematch library inspect {INSTRUCTIONS_APPLIED}{}", w.name)),
        });
    }
    Ok(out)
}

/// An approved text (applied, or over the budget) as an `instructions-applied:<name>` action:
/// decline withdraws it; the reason says how far a withdrawal reaches.
fn text_action(ctx: &AppCtx, w: &instructions::Desired, shown: &str, profile: bool, state: &str) -> Action {
    let id = format!("{INSTRUCTIONS_APPLIED}{}", w.name);
    let mut a = Action::new(&id, "instructions_applied", "system_prompt", &w.name);
    a.entry = w.entry.as_deref().and_then(|id| ctx.ledger.get_entry(id).ok().flatten()).map(|e| ActionEntry::of(&e));
    a.sha256 = Some(w.sha.clone());
    a.target = shown.to_string();
    a.scope = "personal".into();
    a.effect = "always_on".into();
    a.state = state.into();
    a.requires_approval = false;
    a.reload = Some(RELOAD.into());
    a.commands = Commands::for_id(&id, None).without_approve();
    let synced = profile || w.source == instructions::Source::Profile;
    // A withdrawal follows the text wherever the profile carried it: the profile is the same
    // personal harness on every synced machine and every omp profile sharing this root.
    a.reaches = vec![Reach { host: instructions::HOST.into(), target: shown.to_string(), scope: "every omp session on this machine".into(), new: false }];
    if synced {
        a.reaches.push(Reach {
            host: instructions::HOST.into(),
            target: format!("{}/<name>.md", instructions::profile_dir(&ctx.paths).display()),
            scope: "profile-wide: every machine and omp profile that syncs this profile".into(),
            new: false,
        });
    }
    a.reason = format!(
        "Approved personal instructions ({}). Decline withdraws them: the block leaves {shown}{}; Claude Code/Pi/OpenCode sessions stop receiving them; \
         a later approval of the same bytes restores them.",
        match w.source {
            instructions::Source::Ledger => "approved on this machine",
            instructions::Source::Profile => "approved elsewhere and synced through the profile",
        },
        if synced {
            ", and — PROFILE-WIDE — profile/instructions/ at the next `rematch profile sync`, so every synced machine and every omp profile sharing \
             this profile drops them too at its own sync"
        } else {
            ""
        }
    );
    a.diff = format!("--- {shown} (rematch block {})\n{}", w.name, line_diff(&instructions::render_inner(&w.name, &w.text), ""));
    a
}

/// One item per provisional/kept/pinned entry with scripts, hook or agent parts waiting
/// ([`entry_parts`]); `kind` joins them with `+`, `diff` holds each in that order (a script by
/// its bytes' sha256 too, so an edit the text does not show still changes the snapshot).
/// Approve and decline bind the reviewed snapshot like every action; decline retires the entry.
fn entry_actions(ctx: &AppCtx, out: &mut Vec<Action>) -> Result<()> {
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            let parts = entry_parts(ctx, &e);
            if parts.is_empty() {
                continue;
            }
            let mut a = Action::new(&e.id, &library::parts_label(&parts), surface_of(e.kind), &e.name);
            a.entry = Some(ActionEntry::of(&e));
            a.scope = "machine".into();
            a.since = Some(e.status_since.unwrap_or(e.updated_at));
            let mut reason = reason_of(&e);
            let mut targets = Vec::new();
            let mut effects = Vec::new();
            let mut what = Vec::new();
            for part in &parts {
                match part {
                    Approval::Scripts => {
                        let files = library::unapproved_scripts(&e);
                        for (rel, p) in &files {
                            let bytes = std::fs::read(p).unwrap_or_default();
                            a.diff.push_str(&format!("+++ {rel} (sha256 {})\n{}\n", sha256_hex(&bytes), String::from_utf8_lossy(&bytes)));
                        }
                        if let Some(rep) = rematch_core::sandbox::latest_report(&ctx.ledger, &e) {
                            let obs: Vec<String> = rep.scripts.iter().map(rematch_core::sandbox::ScriptDryRun::summary_line).collect();
                            if !obs.is_empty() {
                                reason = format!("{reason}\n\nSandbox dry run:\n{}", obs.join("\n"));
                                a.diff.push_str(&format!("--- sandbox dry run\n{}\n", obs.join("\n")));
                            }
                        }
                        targets.push(e.path.join("scripts").display().to_string());
                        effects.push("executable");
                        what.push(format!("make {} script(s) executable", files.len()));
                    }
                    Approval::Hook => {
                        match library::hook_module(&e) {
                            Ok(m) => a.diff.push_str(&m),
                            Err(err) => {
                                a.diff.push_str(&format!("(hook does not compile: {err:#})\n"));
                                a.block(format!("the hook does not compile ({err:#}); `rematch library decline {}` retires it", e.id));
                            }
                        }
                        if let Some(h) = &e.hook {
                            reason = format!("{reason}\n\nPass condition: {}", h.pass_condition.trim());
                        }
                        targets.push(ctx.paths.hooks_dir().join(format!("{}.ts", e.id)).display().to_string());
                        effects.push("runtime_hook");
                        what.push(format!("run hook ({})", e.hook.as_ref().map(|h| enum_str(&h.mode)).unwrap_or_else(|| "?".into())));
                        a.reload = Some("runs in omp sessions once the extension reloads its hooks (the digest does it at once)".into());
                    }
                    Approval::Agent => {
                        a.diff.push_str(&format!("+++ {AGENT_MD}\n{}\n", std::fs::read_to_string(e.path.join(AGENT_MD)).unwrap_or_default()));
                        targets.push(library::agent_link(&ctx.paths, &e.name).display().to_string());
                        effects.push("subagent");
                        what.push("list the provisional subagent in every session".into());
                        // Approval is where its task-tool line becomes always on: shown blocked
                        // now rather than refused after the tap.
                        let cost = rematch_core::lint::entry_cost(&e.path, Kind::Agent).catalog;
                        if let Err(why) = library::check_always_on(&ctx.ledger, &ctx.paths, &e, cost, "agent's task-tool line") {
                            a.block(format!("{why:#}; or `rematch library decline {}` retires it", e.id));
                        }
                    }
                    Approval::Prompt => {}
                }
            }
            a.reason = reason;
            a.target = targets.join(", ");
            a.effect = effects.join("+");
            a.summary = format!("{} of {} {}", what.join(", "), enum_str(&e.kind), e.name);
            a.sha256 = Some(sha256_hex(a.diff.as_bytes()));
            out.push(a);
        }
    }
    Ok(())
}

/// Fragments awaiting approval into the personal `AGENTS.md` (and matched-session runtime
/// insertion on the other hosts), approved texts over the budget, blocks the user edited, and
/// the file when it is not what it should be (a failed write, an unreadable file).
fn instruction_actions(ctx: &AppCtx, out: &mut Vec<Action>) -> Result<()> {
    let target = instructions::target(&ctx.paths);
    let shown = target.display().to_string();
    let profile = rematch_core::profile::present(&ctx.paths);
    for p in instructions::pending(&ctx.ledger, &ctx.paths)? {
        let e = &p.entry;
        let id = format!("{INSTRUCTIONS}{}", e.id);
        let mut a = Action::new(&id, "instructions", "system_prompt", &e.name);
        a.entry = Some(ActionEntry::of(e));
        a.sha256 = Some(p.fragment_sha256.clone());
        a.target = shown.clone();
        a.scope = "personal".into();
        a.effect = "always_on".into();
        a.reload = Some(RELOAD.into());
        a.since = Some(e.status_since.unwrap_or(e.updated_at));
        // An earlier one-tap of PR-era `prompt` granted runtime insertion only; the personal
        // file is a new reach and is asked for here, never assumed.
        let runtime_done = library::fragment_approved(e);
        a.reaches = vec![
            Reach {
                host: instructions::HOST.into(),
                target: shown.clone(),
                scope: format!(
                    "every omp session on this machine{}",
                    if profile { " and, through `rematch profile sync`, your other machines and every omp profile sharing this profile" } else { "" }
                ),
                new: true,
            },
            Reach {
                host: "claude-code|pi|opencode".into(),
                target: format!("system prompt of main sessions where {} matches", e.name),
                scope: "matched sessions".into(),
                new: !runtime_done,
            },
        ];
        a.reason = format!(
            "{}\n\nAlways-on text in your personal {shown} — not a repo or team file{}. Budget: this block {} B of {} B for every personal instruction. \
             `rematch ship --scope team --entry {}` stays the explicit way to propose it to a repo.",
            reason_of(e),
            if runtime_done { "; you approved these exact bytes for matched-session insertion earlier, the personal file is new" } else { "" },
            p.bytes,
            p.budget,
            e.id
        );
        let new = instructions::render_inner(&e.name, &p.text);
        let old = p.applied.as_ref().map(|d| instructions::render_inner(&d.name, &d.text)).unwrap_or_default();
        a.summary = match &p.applied {
            Some(d) => format!("update personal instructions {} ({} → {} bytes; applied {})", e.name, old.len(), new.len(), sha12(&d.sha)),
            None => format!("add personal instructions {} ({} bytes) to {shown}", e.name, new.len()),
        };
        a.diff = format!(
            "--- {shown} (rematch block {}{})\n+++ {shown} (rematch block {}, fragment {})\n{}",
            e.name,
            p.applied.as_ref().map(|d| format!(", fragment {}", sha12(&d.sha))).unwrap_or_else(|| ", absent".into()),
            e.name,
            sha12(&p.fragment_sha256),
            line_diff(&old, &new)
        );
        if let Some(why) = &p.blocked {
            a.block(format!("{why}; or `rematch library decline {id}`"));
        }
        out.push(a);
    }
    let recs = instructions::records(&ctx.ledger, &ctx.paths)?;
    let set = instructions::desired(&ctx.ledger, &ctx.paths, &recs, true, None)?;
    for w in &set.over {
        let mut a = text_action(ctx, w, &shown, profile, "blocked");
        a.summary = format!("approved personal instructions {} ({} B) do not fit the budget and are not in {shown}", w.name, w.bytes());
        a.block(format!(
            "{} B already applied + {} B > {} B (`limits.always_on_bytes` of {}); withdraw another block (`rematch library inspect instructions-applied:<name>`) or this one",
            set.used,
            w.bytes(),
            set.budget,
            ctx.paths.improver.join("lint.toml").display()
        ));
        out.push(a);
    }
    let plan = instructions::plan_sync(&ctx.ledger, &ctx.paths)?;
    for c in &plan.conflicts {
        let id = format!("{INSTRUCTIONS_CONFLICT}{}", c.name);
        let mut a = Action::new(&id, "instructions_conflict", "system_prompt", &c.name);
        a.state = "conflict".into();
        a.target = shown.clone();
        a.scope = "personal".into();
        a.effect = "always_on".into();
        a.reload = Some(RELOAD.into());
        a.sha256 = c.desired_sha.clone();
        a.reason = instructions::conflict_line(&target, c);
        if c.name == "*" {
            a.summary = format!("{shown}: {}", c.reason);
            a.commands = Commands { inspect: Some(format!("rematch library inspect {id}")), ..Default::default() };
            a.block(format!("{}; rematch leaves the file alone until it is readable and its marker lines parse — fix it by hand", c.reason));
            out.push(a);
            continue;
        }
        let want = set.blocks.get(&c.name);
        a.entry = want.and_then(|w| w.entry.as_deref()).and_then(|id| ctx.ledger.get_entry(id).ok().flatten()).map(|e| ActionEntry::of(&e));
        let synced = profile || want.is_some_and(|w| w.source == instructions::Source::Profile);
        a.summary = format!(
            "personal instructions {} were edited in {shown}; approve {} it, decline keeps your text as your own{}",
            c.name,
            if want.is_some() { "overwrites" } else { "removes" },
            if synced && want.is_some() { " and withdraws the approved text from every synced machine" } else { "" }
        );
        let held = instructions::block_inner(&ctx.paths, &c.name).unwrap_or_default();
        let new = want.map(|w| instructions::render_inner(&w.name, &w.text)).unwrap_or_default();
        // The snapshot binds both sides: the edited block as it is and what approve writes.
        a.diff = format!(
            "--- {shown} (block {} as edited, inner sha256 {})\n+++ {shown} (after approve, fragment {})\n{}",
            c.name,
            c.held_sha.as_deref().unwrap_or("?"),
            c.desired_sha.as_deref().unwrap_or("none: removed"),
            line_diff(&held, &new)
        );
        out.push(a);
    }
    // The file is not what it should be: a write failed (read-only file or dir), or no sync ran
    // since the state changed (`REMATCH=0` in the environment).
    if plan.changed {
        let id = INSTRUCTIONS_SYNC.to_string();
        let mut a = Action::new(&id, "instructions_sync", "system_prompt", "personal AGENTS.md");
        a.target = shown.clone();
        a.scope = "personal".into();
        a.effect = "always_on".into();
        a.reload = Some(RELOAD.into());
        a.commands = Commands::for_id(&id, None);
        a.commands.decline = None;
        a.commands.decline_argv = None;
        let err = instructions::last_error(&ctx.paths);
        a.summary = format!("{shown} is out of date: {}", plan.lines().first().cloned().unwrap_or_default());
        a.reason = format!(
            "New omp sessions load {shown} as it is now, not as your approvals and rematch's on/off state say.{} Approve writes it now.",
            err.as_deref().map(|e| format!(" The last write failed: {e} — fix the permissions first.")).unwrap_or_default()
        );
        a.diff = plan.lines().join("\n");
        out.push(a);
    }
    // Approved texts the runtime insertion of Claude Code/Pi/OpenCode cannot read: those hosts
    // drop them (failing open) while the approval says they are on. Clears once the file reads.
    for f in library::unreadable_fragments(&ctx.ledger)? {
        let id = format!("{FRAGMENT_UNREADABLE}{}", f.id);
        let mut a = Action::new(&id, "instructions_unreadable", "system_prompt", &f.name);
        a.entry = ctx.ledger.get_entry(&f.id)?.map(|e| ActionEntry::of(&e));
        a.target = f.path.display().to_string();
        a.scope = "personal".into();
        a.effect = "always_on".into();
        a.requires_approval = false;
        a.reload = Some(RELOAD.into());
        a.summary = format!("approved personal instructions {} are not sent to Claude Code, Pi or OpenCode: {} cannot be read", f.name, f.path.display());
        a.reason = format!(
            "The matched-session insertion of the other hosts reads {} each session and skips it while it cannot be read. \
             Decline withdraws that approval (the entry and the personal AGENTS.md block, if any, stay).",
            f.path.display()
        );
        a.diff = format!("--- {}\n{}\n", f.path.display(), f.error);
        a.block(format!("{}; fix the file (its permissions), or decline to withdraw the approval", f.error));
        out.push(a);
    }
    Ok(())
}

fn export_actions(ctx: &AppCtx, out: &mut Vec<Action>) -> Result<()> {
    for x in crate::scope::export::pending(ctx)? {
        let entry_id = x.id.trim_start_matches(EXPORT).to_string();
        let id = format!("{EXPORT}{entry_id}");
        let e = ctx.ledger.get_entry(&entry_id)?;
        let mut a = Action::new(&id, "export", e.as_ref().map_or("skills", |e| surface_of(e.kind)), &x.name);
        a.entry = e.as_ref().map(ActionEntry::of);
        a.target = ctx.paths.export.display().to_string();
        a.scope = "home_export".into();
        a.effect = "copy".into();
        a.reason = x.reason.clone();
        a.summary = format!(
            "export {} to {}{}",
            x.name,
            ctx.paths.export.display(),
            if x.dropped_facts.is_empty() { String::new() } else { format!(" (drops {} fact(s))", x.dropped_facts.len()) }
        );
        a.sha256 = Some(sha256_hex(x.diff.as_bytes()));
        a.diff = x.diff;
        a.since = e.as_ref().map(|e| e.updated_at);
        out.push(a);
    }
    Ok(())
}

/// Ship candidates of kept/pinned entries (`ship::ready`: team when the predicate recurred in
/// ≥ 2 repos, fork with a `fork_path`) that the user has not declined for these bytes, plus
/// fork text without a target. Approving builds the local branch only; nothing is pushed.
fn ship_actions(ctx: &AppCtx, out: &mut Vec<Action>) -> Result<()> {
    let mut offered = std::collections::BTreeSet::new();
    for r in crate::scope::ship::ready(ctx)? {
        let Some(e) = ctx.ledger.get_entry(&r.id)? else { continue };
        offered.insert(e.id.clone());
        let fork = r.scope == "fork";
        let (sha, diff) = if fork {
            let content = crate::scope::ship::fork_content(&e, &r.detail).unwrap_or_default();
            (sha256_hex(content.as_bytes()), format!("+++ {}\n{}", r.detail, line_diff("", &content)))
        } else {
            let sha = e.content_sha256.clone().or_else(|| library::content_sha256(&e.path).ok()).unwrap_or_default();
            let fragment = std::fs::read_to_string(e.path.join(AGENTS_FRAGMENT)).ok();
            let mut d = format!("+++ .omp/{}/{}\n", if e.kind == Kind::Agent { "agents" } else { "skills" }, e.name);
            if let Some(f) = &fragment {
                d.push_str(&format!("+++ AGENTS.md (section `## rematch: {}`)\n{}", e.name, line_diff("", f)));
            }
            (sha, d)
        };
        if ship_declined(ctx, &e, r.scope, &sha)? {
            continue;
        }
        let id = format!("{}{}", if fork { FORK } else { SHIP_TEAM }, e.id);
        let surface = if fork {
            if r.detail.to_ascii_lowercase().contains("tool") {
                "tool_guidance"
            } else {
                "system_prompt"
            }
        } else {
            surface_of(e.kind)
        };
        let mut a = Action::new(&id, if fork { "fork" } else { "ship" }, surface, &e.name);
        a.entry = Some(ActionEntry::of(&e));
        a.scope = r.scope.into();
        a.effect = "external_branch".into();
        a.since = Some(e.status_since.unwrap_or(e.updated_at));
        a.sha256 = Some(sha.clone());
        a.commands = Commands::for_id(&id, Some(sha12(&sha)));
        a.diff = diff;
        a.reason = format!(
            "{}\n\nApproving builds branch rematch/{}-… from the target's HEAD in a temporary worktree (your checkout is untouched) and never pushes; \
             `rematch ship --scope {} --entry {} --push [--pr]` publishes it. Declining stops this offer until the entry changes.",
            reason_of(&e),
            e.name,
            r.scope,
            e.id
        );
        if fork {
            match &e.fork_repo {
                Some(repo) => {
                    a.target = format!("{}:{}", repo.display(), r.detail);
                    a.summary = format!("ship {} to fork {}:{} as a local branch", e.name, repo.display(), r.detail);
                }
                None => {
                    a.target = format!("<fork checkout>:{}", r.detail);
                    a.summary = format!("ship {} to fork file {}", e.name, r.detail);
                    a.block(format!(
                        "no fork checkout configured: `rematch library set-fork-path {} {} --repo <fork checkout>`",
                        e.id, r.detail
                    ));
                }
            }
        } else {
            let repo = crate::scope::origin_repo(ctx, &e)?;
            a.target = repo.as_ref().map(|r| r.display().to_string()).unwrap_or_else(|| "<team repo>".into());
            a.summary = format!("propose {} to the team repo {} ({})", e.name, a.target, r.detail);
            if repo.is_none() {
                a.block(format!("no originating repo in the ledger: `rematch ship --scope team --entry {} --repo <repo>`", e.id));
            }
        }
        out.push(a);
    }
    // Fork text under `fork/` that has no target file: the user names one.
    for st in [Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            if offered.contains(&e.id) || library::is_builtin(&e) || crate::reflect::is_foreign(ctx, &e) {
                continue;
            }
            if e.fork_path.is_some() || library::load_entry(&e.path).ok().and_then(|d| d.fork_path).is_some() {
                continue;
            }
            let files = files_under(&e.path.join("fork"));
            if files.is_empty() {
                continue;
            }
            let rels: Vec<String> = files.iter().map(|p| p.strip_prefix(&e.path).unwrap_or(p).to_string_lossy().replace('\\', "/")).collect();
            let id = format!("{FORK}{}", e.id);
            let mut a = Action::new(&id, "fork", "system_prompt", &e.name);
            a.entry = Some(ActionEntry::of(&e));
            a.scope = "fork".into();
            a.effect = "external_branch".into();
            a.since = Some(e.updated_at);
            a.reason = reason_of(&e);
            a.target = "<fork checkout>:<file>".into();
            a.summary = format!("fork text of {} ({}) has no target file", e.name, rels.join(", "));
            a.diff = files.iter().zip(&rels).map(|(p, r)| format!("+++ {r}\n{}\n", std::fs::read_to_string(p).unwrap_or_default())).collect();
            a.block(format!(
                "a fork change needs an explicit target: `rematch library set-fork-path {} <repo-relative file> --repo <fork checkout>`",
                e.id
            ));
            out.push(a);
        }
    }
    Ok(())
}

/// The user declined shipping this entry to `scope` for these bytes.
fn ship_declined(ctx: &AppCtx, e: &Entry, scope: &str, sha: &str) -> Result<bool> {
    for r in ctx.ledger.list_receipts(Some(&e.id))? {
        let Some(b) = ctx.ledger.get_blob(&r.receipt.certificate).ok().flatten() else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&b) else { continue };
        if v["what"] == DECLINE_SHIP && v["scope"] == scope && v["sha256"] == sha {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------------------------
// Trials and per-entry surfaces

/// Automatic trials running now (provisional, loading): observable, never a forced approval.
pub fn trials(ctx: &AppCtx) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })? {
        if e.kind == Kind::Lesson || !entry_parts(ctx, &e).is_empty() {
            continue;
        }
        let c = ctx.ledger.counters(&e.id)?;
        out.push(json!({
            "id": e.id, "name": e.name, "kind": e.kind, "surface": surface_of(e.kind), "branch": e.branch, "version": e.version,
            "since": e.status_since.unwrap_or(e.created_at), "content_sha256": e.content_sha256, "counters": c,
            "commands": {"disable": format!("rematch library disable {}", e.id), "why": "rematch library why"},
        }));
    }
    Ok(out)
}

/// Per-entry surface states for `library ls`: the entry's own surface and, when it has them,
/// scripts, personal instructions and a fork target.
pub struct SurfaceView {
    recs: Vec<instructions::InstructionRecord>,
    exposure: instructions::Exposure,
}

impl SurfaceView {
    pub fn load(ctx: &AppCtx) -> Result<SurfaceView> {
        Ok(SurfaceView { recs: instructions::records(&ctx.ledger, &ctx.paths)?, exposure: instructions::exposure(&ctx.ledger, &ctx.paths)? })
    }

    /// The state of the entry's personal text: `applied`, `awaiting approval`, … (`None`
    /// without a fragment).
    pub fn instructions_state(&self, e: &Entry) -> Option<String> {
        let (sha, _) = instructions::fragment(e)?;
        let applied = self.exposure.exposed.iter().find(|x| x.name == e.name);
        Some(match instructions::decision(&self.recs, &e.name, &sha) {
            Some(instructions::Decision::Declined) => "declined".to_string(),
            Some(instructions::Decision::Approved) if applied.is_some_and(|x| x.sha == sha) => "applied".to_string(),
            Some(instructions::Decision::Approved) if self.exposure.conflicts.contains(&e.name) => "approved; block edited by you (conflict)".to_string(),
            Some(instructions::Decision::Approved) => "approved; not in the file (rematch off, entry not loadable, or another text applied)".to_string(),
            None if instructions::loadable(e) => match applied {
                Some(x) => format!("revision awaiting approval (applied: {})", sha12(&x.sha)),
                None => "awaiting approval".to_string(),
            },
            None => "not offered (entry not loadable)".to_string(),
        })
    }

    /// Per-entry surfaces, built only from ledger-held data (the entry's recorded path and
    /// fields), so a library rebuilt on another root (`lake rebuild`) lists the same values:
    /// machine-local targets (the hooks dir, the personal AGENTS.md) are named by role here and
    /// given in full by the action queue and the top-level `instructions.target`.
    pub fn of(&self, ctx: &AppCtx, e: &Entry) -> Vec<Value> {
        let mut out = Vec::new();
        let state = enum_str(&e.status);
        let parts = entry_parts(ctx, e);
        let waits = |p: Approval| parts.contains(&p);
        out.push(match e.kind {
            Kind::Hook => json!({"surface": "hooks", "state": if waits(Approval::Hook) { "awaiting approval".into() } else { state.clone() },
                                 "target": format!("<agent_dir>/extensions/rematch-hooks/{}.ts", e.id)}),
            Kind::Agent => json!({"surface": "subagents", "state": if waits(Approval::Agent) { "awaiting approval".into() } else { state.clone() },
                                  "target": e.path.join(AGENT_MD)}),
            k => json!({"surface": surface_of(k), "state": state, "target": e.path, "effect": "catalog description always; body on demand"}),
        });
        if e.path.join("scripts").is_dir() {
            out.push(json!({"surface": "scripts", "state": if waits(Approval::Scripts) { "awaiting approval" } else { "approved" },
                            "files": files_under(&e.path.join("scripts")).len(), "approved": e.approved_scripts.len()}));
        }
        if let Some(st) = self.instructions_state(e) {
            out.push(json!({"surface": "system_prompt", "state": st, "target": "<agent_dir>/AGENTS.md", "effect": "always_on",
                            "runtime_insertion": library::fragment_approved(e)}));
        }
        if let Some(fp) = &e.fork_path {
            out.push(json!({"surface": if fp.to_ascii_lowercase().contains("tool") { "tool_guidance" } else { "system_prompt" },
                            "state": if e.fork_commit.is_some() { "shipped (fork branch)" } else { "fork candidate" },
                            "target": format!("{}:{fp}", e.fork_repo.as_ref().map(|r| r.display().to_string()).unwrap_or_else(|| "<fork checkout>".into()))}));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Commands

/// A pending or applied action by id, as computed now.
pub fn find(ctx: &AppCtx, id: &str) -> Result<Action> {
    pending(ctx)?
        .into_iter()
        .chain(applied(ctx)?)
        .find(|a| a.id == id)
        .ok_or_else(|| anyhow!("no pending or applied action {id:?} (`rematch status` lists them)"))
}

/// `library inspect <id>`: the action with its whole diff.
pub fn inspect(ctx: &AppCtx, id: &str) -> Result<Value> {
    let a = find(ctx, id)?;
    let mut lines = vec![a.line(), format!("target: {} · scope {} · surface {} · effect {}", a.target, a.scope, a.surface, a.effect)];
    for r in &a.reaches {
        lines.push(format!("reach: {} → {} ({}){}", r.host, r.target, r.scope, if r.new { "" } else { " — already approved" }));
    }
    if let Some(r) = &a.reload {
        lines.push(r.clone());
    }
    lines.push(a.reason.clone());
    lines.extend(a.commands.approve.iter().map(|c| format!("approve: {c}")));
    lines.extend(a.commands.decline.iter().map(|c| format!("decline: {c}")));
    lines.push(a.diff.clone());
    Ok(json!({"action": a, "lines": lines}))
}

/// The action `id` as it is now, provided `sha` is the hash of the snapshot the user reviewed
/// (`review_sha256`, carried by the action's commands).
fn reviewed(ctx: &AppCtx, id: &str, verb: &str, sha: Option<&str>) -> Result<Action> {
    let a = find(ctx, id)?;
    let Some(given) = sha.map(|s| s.trim().to_ascii_lowercase()) else {
        bail!("`library {verb}` binds the reviewed snapshot: run the command `rematch library inspect {id}` shows (with --sha)");
    };
    if given != a.review_sha256 {
        bail!("{id} changed since it was shown (target, scope, reach or diff); review it again: `rematch library inspect {id}`");
    }
    Ok(a)
}

/// `library approve <id> --sha <review sha256>`: the action as reviewed, applied.
pub fn approve(ctx: &AppCtx, id: &str, sha: Option<&str>) -> Result<Value> {
    let a = reviewed(ctx, id, "approve", sha)?;
    if a.commands.approve_argv.is_none() {
        bail!("{id} cannot be approved: {}", a.blocked.as_deref().unwrap_or("only declined or inspected"));
    }
    let content = a.sha256.clone().unwrap_or_default();
    if let Some(entry) = id.strip_prefix(INSTRUCTIONS) {
        let (receipt, sync) = instructions::approve(&ctx.ledger, &ctx.paths, entry, &content)?;
        let mut lines = vec![format!("approved personal instructions {}", a.name)];
        lines.extend(sync.lines());
        if ctx.paths.is_off() {
            lines.push("rematch is off: the text is applied when you run `rematch on`".into());
        }
        return Ok(json!({"receipt": receipt, "instructions": sync, "lines": lines}));
    }
    if let Some(name) = id.strip_prefix(INSTRUCTIONS_CONFLICT) {
        let (held, want) = conflict_shas(ctx, name)?;
        let sync = instructions::resolve(&ctx.ledger, &ctx.paths, name, instructions::Resolve::Restore, &held, want.as_deref())?;
        return Ok(json!({"instructions": sync, "lines": sync.lines()}));
    }
    if id == INSTRUCTIONS_SYNC {
        let sync = instructions::sync(&ctx.ledger, &ctx.paths)?;
        let mut lines = sync.lines();
        if lines.is_empty() {
            lines.push(format!("{} is up to date", sync.target.display()));
        }
        return Ok(json!({"instructions": sync, "lines": lines}));
    }
    if id.starts_with(FORK) || id.starts_with(SHIP_TEAM) {
        return approve_ship(ctx, &a);
    }
    if id.starts_with(PROBE) {
        return crate::probes::approve_action(ctx, id, a.sha256.as_deref());
    }
    if let Some(entry) = id.strip_prefix(EXPORT) {
        return crate::scope::export::approve(ctx, entry);
    }
    approve_entry(ctx, &a)
}

/// `library decline <id> --sha <review sha256>`.
pub fn decline(ctx: &AppCtx, id: &str, sha: Option<&str>) -> Result<Value> {
    let a = reviewed(ctx, id, "decline", sha)?;
    if a.commands.decline_argv.is_none() {
        bail!("{id} has nothing to decline: {}", a.blocked.as_deref().unwrap_or("approve or inspect it"));
    }
    let content = a.sha256.clone().unwrap_or_default();
    if let Some(entry) = id.strip_prefix(INSTRUCTIONS) {
        let d = instructions::decline(&ctx.ledger, &ctx.paths, entry, &content)?;
        let mut lines = vec![format!("declined personal instructions {} ({}); the entry stays", d.name, sha12(&d.fragment_sha256))];
        lines.extend(d.sync.lines());
        return Ok(json!({"declined": d, "lines": lines}));
    }
    if let Some(name) = id.strip_prefix(INSTRUCTIONS_APPLIED) {
        // The reviewed snapshot fixed the text: the runtime reach goes first, so a failed file
        // write leaves no half-withdrawn text behind (the drift stays as `instructions-sync`).
        let runtime = revoke_runtime(ctx, name, &content)?;
        let d = instructions::withdraw(&ctx.ledger, &ctx.paths, name, &content)?;
        let mut lines = vec![format!("withdrew personal instructions {name} ({})", sha12(&d.fragment_sha256))];
        lines.extend(runtime);
        if rematch_core::profile::present(&ctx.paths) {
            lines.push(
                "profile-wide: the next `rematch profile sync` removes it from profile/instructions/, and every synced machine and omp profile drops it at its own sync"
                    .into(),
            );
        }
        lines.extend(d.sync.lines());
        return Ok(json!({"declined": d, "lines": lines}));
    }
    if let Some(entry) = id.strip_prefix(FRAGMENT_UNREADABLE) {
        let mut e = ctx.ledger.find_entry(entry)?.ok_or_else(|| anyhow!("no entry {entry:?}"))?;
        if crate::reflect::is_foreign(ctx, &e) {
            bail!("{} is not a rematch-owned entry; nothing to withdraw", e.name);
        }
        e.approved_fragment = None;
        e.updated_at = ctx.now;
        crate::scope::save_entry(ctx, &e)?;
        return Ok(json!({
            "entry": e.id,
            "lines": [format!("withdrew the runtime approval of {}: it no longer joins matched Claude Code/Pi/OpenCode sessions", e.name)],
        }));
    }
    if let Some(name) = id.strip_prefix(INSTRUCTIONS_CONFLICT) {
        let (held, want) = conflict_shas(ctx, name)?;
        let sync = instructions::resolve(&ctx.ledger, &ctx.paths, name, instructions::Resolve::Release, &held, want.as_deref())?;
        let mut lines = vec![format!("kept your edited text of {name} as your own; rematch no longer manages it")];
        if let Some(w) = &want {
            lines.extend(revoke_runtime(ctx, name, w)?);
        }
        lines.extend(sync.lines());
        return Ok(json!({"instructions": sync, "lines": lines}));
    }
    if id.starts_with(FORK) || id.starts_with(SHIP_TEAM) {
        return decline_ship(ctx, &a);
    }
    if id.starts_with(PROBE) {
        return crate::probes::decline_action(ctx, id);
    }
    if let Some(entry) = id.strip_prefix(EXPORT) {
        return crate::scope::export::decline(ctx, entry);
    }
    decline_entry(ctx, &a)
}

/// The edited block's inner sha and the fragment sha rematch would write, as planned now.
fn conflict_shas(ctx: &AppCtx, name: &str) -> Result<(String, Option<String>)> {
    let plan = instructions::plan_sync(&ctx.ledger, &ctx.paths)?;
    let c = plan.conflicts.into_iter().find(|c| c.name == name).ok_or_else(|| anyhow!("block {name} is not in conflict"))?;
    Ok((c.held_sha.unwrap_or_default(), c.desired_sha))
}

/// The entry's scripts, hook and agent parts, as reviewed.
fn approve_entry(ctx: &AppCtx, a: &Action) -> Result<Value> {
    let e = ctx.ledger.find_entry(&a.id)?.ok_or_else(|| anyhow!("no entry {:?}", a.id))?;
    if crate::reflect::is_foreign(ctx, &e) {
        bail!("{} is not a rematch-owned entry; nothing to approve", e.name);
    }
    let parts = entry_parts(ctx, &e);
    let receipts = parts.iter().map(|p| library::approve(&ctx.ledger, &ctx.paths, &e.id, *p)).collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "receipts": receipts,
        "approved": parts,
        "lines": [format!("approved {} of {}", library::parts_label(&parts), e.name)],
    }))
}

/// Declining an entry's executable parts retires the entry.
fn decline_entry(ctx: &AppCtx, a: &Action) -> Result<Value> {
    let e = ctx.ledger.find_entry(&a.id)?.ok_or_else(|| anyhow!("no entry {:?}", a.id))?;
    if crate::reflect::is_foreign(ctx, &e) {
        bail!("{} is not a rematch-owned entry; nothing to decline", e.name);
    }
    let parts = entry_parts(ctx, &e);
    let r = library::decline(&ctx.ledger, &ctx.paths, &e.id, &parts)?;
    Ok(json!({"entry": r, "lines": [format!("declined {} of {}; entry retired", library::parts_label(&parts), e.name)]}))
}

/// A withdrawn text also leaves the matched-session insertion of the other hosts: clear
/// `approved_fragment` of the rematch-owned entries of `name` that hold exactly it.
fn revoke_runtime(ctx: &AppCtx, name: &str, sha: &str) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for mut e in ctx.ledger.list_entries(&EntryFilter { name: Some(name.to_string()), ..Default::default() })? {
        if e.approved_fragment.as_deref() != Some(sha) || crate::reflect::is_foreign(ctx, &e) {
            continue;
        }
        e.approved_fragment = None;
        e.updated_at = ctx.now;
        crate::scope::save_entry(ctx, &e)?;
        lines.push(format!("{} no longer joins matched Claude Code/Pi/OpenCode sessions", e.name));
    }
    Ok(lines)
}

fn approve_ship(ctx: &AppCtx, a: &Action) -> Result<Value> {
    let e = a.entry.as_ref().ok_or_else(|| anyhow!("ship action without an entry"))?;
    let scope = if a.scope == "fork" { crate::scope::ship::ShipScope::Fork } else { crate::scope::ship::ShipScope::Team };
    let args = crate::scope::ship::ShipArgs { scope, repo: None, entries: vec![e.id.clone()], push: false, pr: false, remote: "origin".into() };
    let mut out = crate::scope::ship::cmd_ship(ctx, args)?;
    out["lines"] = json!([format!("built the {} branch of {} in {} (not pushed)", a.scope, e.name, a.target)]);
    Ok(out)
}

fn decline_ship(ctx: &AppCtx, a: &Action) -> Result<Value> {
    let e = a.entry.as_ref().ok_or_else(|| anyhow!("ship action without an entry"))?;
    let sha = a.sha256.clone().unwrap_or_else(|| sha256_hex(a.diff.as_bytes()));
    let cert = json!({"what": DECLINE_SHIP, "scope": a.scope, "entry": e.id, "sha256": sha, "target": a.target, "at": now_ms()});
    let certificate = ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
    let receipt = ctx.ledger.insert_receipt(&Receipt { certificate, entry: e.id.clone(), activation: Activation::Prompted })?;
    Ok(json!({"declined": a.id, "receipt": receipt.id, "lines": [format!("declined shipping {} ({}); offered again when it changes", e.name, a.scope)]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, stage};

    #[test]
    fn line_diff_marks_changes_only() {
        assert_eq!(line_diff("a\nb\nc\n", "a\nx\nc\n"), " a\n-b\n+x\n c\n");
        assert_eq!(line_diff("", "a\n"), "+a\n");
    }

    fn review(ctx: &AppCtx, id: &str) -> String {
        let a = find(ctx, id).unwrap();
        assert_eq!(a.commands.approve_argv.as_ref().or(a.commands.decline_argv.as_ref()).unwrap().last(), Some(&a.review_sha256), "{a:?}");
        assert_eq!(a.review_sha256.len(), 64);
        a.review_sha256
    }

    fn stale(r: Result<Value>) {
        let err = r.unwrap_err();
        assert!(format!("{err:#}").contains("changed since it was shown"), "{err:#}");
    }

    #[test]
    fn an_export_binds_the_reviewed_content() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage(ctx, &entry("e-inv", "verify-before-claim", "quant"), &[]);
        let e = keep(ctx, &e.id);
        let id = format!("{EXPORT}{}", e.id);
        let seen = review(ctx, &id);
        assert!(approve(ctx, &id, None).is_err(), "no hash, no export");
        std::fs::create_dir_all(e.path.join("units")).unwrap();
        std::fs::write(e.path.join("units/extra.md"), "## Procedure\n\n1. Re-run the check.\n").unwrap();
        stale(approve(ctx, &id, Some(&seen)));
        stale(decline(ctx, &id, Some(&seen)));
        assert!(!ctx.paths.export.join(&e.id).exists(), "nothing exported");
        approve(ctx, &id, Some(&review(ctx, &id))).unwrap();
        assert!(ctx.paths.export.join(&e.id).join("units/extra.md").is_file());
    }

    #[test]
    fn text_and_conflict_decisions_bind_both_the_edited_block_and_the_desired_text() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage(ctx, &entry("e-text", "text-rule", "quant"), &[(AGENTS_FRAGMENT, "Run the tests.\n")]);
        let fragment = e.path.join(AGENTS_FRAGMENT);
        let id = format!("{INSTRUCTIONS}{}", e.id);
        let seen = review(ctx, &id);
        std::fs::write(&fragment, "Run the tests twice.\n").unwrap();
        stale(approve(ctx, &id, Some(&seen)));
        approve(ctx, &id, Some(&review(ctx, &id))).unwrap();
        let file = instructions::target(&ctx.paths);
        let read = || std::fs::read_to_string(&file).unwrap();
        assert!(read().contains("Run the tests twice."));

        // Edited, reviewed, edited again: neither restore nor release goes through.
        std::fs::write(&file, read().replace("twice", "three times")).unwrap();
        let cid = format!("{INSTRUCTIONS_CONFLICT}text-rule");
        let seen = review(ctx, &cid);
        assert!(find(ctx, &cid).unwrap().diff.contains("three times"));
        let again = read().replace("three times", "four times");
        std::fs::write(&file, &again).unwrap();
        stale(approve(ctx, &cid, Some(&seen)));
        stale(decline(ctx, &cid, Some(&seen)));
        assert_eq!(read(), again);
        assert!(instructions::last_error(&ctx.paths).is_none(), "a refused review is no failed write");

        // What a restore writes changed (a revision approved meanwhile): refused too.
        let seen = review(ctx, &cid);
        std::fs::write(&fragment, "Run the tests five times.\n").unwrap();
        approve(ctx, &id, Some(&review(ctx, &id))).unwrap();
        assert_eq!(read(), again, "the edited block stays");
        stale(approve(ctx, &cid, Some(&seen)));
        assert_eq!(read(), again);
        approve(ctx, &cid, Some(&review(ctx, &cid))).unwrap();
        assert!(read().contains("Run the tests five times.") && !read().contains("four times"));

        // Applied: inspectable and withdrawable under its own id, bound like the rest.
        let aid = format!("{INSTRUCTIONS_APPLIED}text-rule");
        let applied = find(ctx, &aid).unwrap();
        assert_eq!((applied.state.as_str(), applied.commands.approve_argv.is_none()), ("applied", true));
        assert!(decline(ctx, &aid, Some("0".repeat(64).as_str())).is_err());
        decline(ctx, &aid, Some(&applied.review_sha256)).unwrap();
        assert!(!file.exists(), "the last block gone, the file rematch created goes too");
        assert!(ctx.ledger.get_entry(&e.id).unwrap().unwrap().approved_fragment.is_none(), "the runtime reach goes too");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_approved_fragment_is_a_blocked_action_until_withdrawn() {
        use std::os::unix::fs::PermissionsExt;
        let t = env();
        let ctx = &t.ctx;
        let e = stage(ctx, &entry("e-text", "text-rule", "quant"), &[(AGENTS_FRAGMENT, "Run the tests.\n")]);
        let id = format!("{INSTRUCTIONS}{}", e.id);
        approve(ctx, &id, Some(&review(ctx, &id))).unwrap();
        let fragment = e.path.join(AGENTS_FRAGMENT);
        std::fs::set_permissions(&fragment, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&fragment).is_ok() {
            return; // running as root: permissions do not bind
        }
        let uid = format!("{FRAGMENT_UNREADABLE}{}", e.id);
        let a = find(ctx, &uid).unwrap();
        assert_eq!((a.state.as_str(), a.commands.approve_argv.is_none(), a.commands.decline_argv.is_some()), ("blocked", true, true), "{a:?}");
        decline(ctx, &uid, Some(&a.review_sha256)).unwrap();
        assert!(find(ctx, &uid).is_err());
        assert!(ctx.ledger.get_entry(&e.id).unwrap().unwrap().approved_fragment.is_none());
        std::fs::set_permissions(&fragment, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
