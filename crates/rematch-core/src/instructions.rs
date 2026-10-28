//! Personal instructions (DESIGN §16 "System prompt", personal scope): the exact
//! `AGENTS.fragment.md` text the user approved, written into the user's own
//! `<agent_dir>/AGENTS.md` — the user-level context file omp's builtin provider loads
//! (`getAgentDir()/AGENTS.md`) in every session. A repo `AGENTS.md` is never written here
//! (`ship --scope team` is the only path there).
//!
//! Each entry name owns at most one block, between marker lines rematch writes at the start of
//! a line, outside any code fence:
//!
//! ```text
//! <!-- rematch:begin name=<name> sha256=<inner sha> fragment=<fragment sha> pad=<n> -->
//! ## rematch: <name>
//!
//! <approved fragment text>
//! <!-- rematch:end name=<name> -->
//! ```
//!
//! `sha256` is the sha256 of the lines between the markers (CRLF read as LF): a block whose
//! inner text no longer hashes to it was edited by the user and is never rewritten or removed
//! automatically (a conflict, surfaced as an action). `pad` counts the line breaks rematch put
//! before the block so a removal restores the bytes it found. Bytes outside the markers are
//! never touched; marker-like lines inside the user's fenced code, or malformed ones, are the
//! user's text. A fragment holding `<!-- rematch:` anywhere is refused.
//!
//! Approvals and declines are receipts (`Activation::Prompted`) whose certificate is the blob of
//! an [`InstructionRecord`] about one `(name, exact bytes)` and one personal file (`target`):
//! `REMATCH_ROOT` and its ledger are shared by every omp profile, each with its own agent dir,
//! and a decision about one file never decides another. The approved text itself is a blob
//! keyed by its sha256. An approved text is materialized only while a loadable, rematch-owned
//! entry of its name holds those bytes, or is the entry it was approved for (whose revised
//! fragment then awaits its own approval while the approved text stays). Every block together
//! stays within `limits.always_on_bytes` of `improver/lint.toml` (and, at approval, next to the
//! rest rematch keeps always on: `library::check_always_on`): an approval that would not fit is
//! refused, and a text that arrives over the budget (the profile, a lowered limit) waits as a
//! blocked action while the blocks already in the file keep their place.
//!
//! The profile (DESIGN §20) carries the canonical approved text as
//! `profile/instructions/<name>.md` and the latest profile-wide decision about it as
//! `<name>.decision`; another machine (or omp profile) applies it into its own agent dir, and a
//! withdrawal anywhere withdraws it everywhere until someone approves those bytes again. An omp
//! profile `policy.json` `skip_profiles` names takes no profile text and contributes none.
//! Every write of the file and of its state (`REMATCH_ROOT/personal-instructions/<target
//! hash>.json`) holds the OS lock `REMATCH_ROOT/.instructions.lock`; a concurrent external edit
//! is still detected by re-reading the file before the rename. A failed write is recorded
//! (`last_error`, per file) and the drift between the file and what it should hold stays visible
//! until a sync succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::ledger::{now_ms, sha256_hex, EntryFilter, Ledger};
use crate::library::{self, validate_name, Approval, AGENTS_FRAGMENT};
use crate::paths::RematchPaths;
use crate::types::{Activation, Entry, EntryId, Kind, Receipt, Sha256Hex, Status, UnixMs};

/// omp's user-level context file name in the agent dir.
pub const AGENTS_MD: &str = "AGENTS.md";
/// The one host whose user-level context file is `<agent_dir>/AGENTS.md`.
pub const HOST: &str = "omp";
/// `InstructionRecord::what`.
pub const RECORD_WHAT: &str = "personal_instructions";
/// Receipt entry of a decision about a profile-only block (no local entry): `personal:<name>`.
pub const PROFILE_ENTRY_PREFIX: &str = "personal:";
/// `REMATCH_ROOT/<this>/<16 hex of sha256(target)>.json`, one per personal file (every omp
/// profile has its own agent dir under the one shared root): whether rematch created that file
/// (removed again only then, and only when it is empty after the last block goes) and its last
/// failed write. A state without a matching `target` is not this file's: never trusted to delete.
const STATE_DIR: &str = "personal-instructions";
/// `REMATCH_ROOT/<this>`: the single state of earlier builds, without a target. Never read.
const LEGACY_STATE_FILE: &str = "personal-instructions.json";
/// `REMATCH_ROOT/<this>`: the OS lock every writer of the file and the state holds.
const LOCK_FILE: &str = ".instructions.lock";
const BEGIN: &str = "<!-- rematch:begin ";
const END: &str = "<!-- rematch:end ";
const CLOSE: &str = " -->";
const MARKER_PREFIX: &str = "<!-- rematch:";

/// `<agent_dir>/AGENTS.md`.
pub fn target(paths: &RematchPaths) -> PathBuf {
    paths.agent_dir.join(AGENTS_MD)
}

/// The aggregate budget of every block (`limits.always_on_bytes`, as `prompt_fragments`).
pub fn budget(paths: &RematchPaths) -> usize {
    crate::lint::LintConfig::load(&paths.improver.join("lint.toml")).unwrap_or_default().limits.always_on_bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Declined,
}

/// The certificate blob of an instruction receipt (a user decision about one exact text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionRecord {
    pub what: String,
    pub decision: Decision,
    /// The entry the text came from; `None` for a profile-only block.
    pub entry: Option<EntryId>,
    pub name: String,
    pub version: Option<u32>,
    /// sha256 of the exact fragment bytes (also the blob holding them).
    pub fragment_sha256: Sha256Hex,
    pub target: PathBuf,
    pub at: UnixMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
}

/// Where a desired block's text comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Approved on this machine for a loadable entry.
    Ledger,
    /// `profile/instructions/<name>.md`, approved on some machine and synced.
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Desired {
    pub name: String,
    pub sha: Sha256Hex,
    pub text: String,
    /// The local entry credited with the exposure, if any.
    pub entry: Option<EntryId>,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
}

impl Desired {
    /// Rendered size of the block (markers included, LF).
    pub fn bytes(&self) -> usize {
        render_block(self, 0, "\n").len()
    }
}

/// What the file should hold: the blocks within the budget and the approved texts that did not
/// fit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DesiredSet {
    pub blocks: BTreeMap<String, Desired>,
    pub over: Vec<Desired>,
    pub budget: usize,
    /// Bytes of the admitted blocks plus edited blocks that stay.
    pub used: usize,
}

/// One block omp loads now: unedited, under rematch's markers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposed {
    pub id: Option<EntryId>,
    pub name: String,
    /// sha256 of the approved fragment text.
    pub sha: Sha256Hex,
    /// `ledger` | `profile` | `stale` (still in the file, no longer desired: a sync removes it).
    pub source: String,
    /// Rendered block bytes, markers included.
    pub bytes: usize,
}

/// What the personal context file holds for rematch now (read-only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposure {
    pub host: String,
    pub target: PathBuf,
    pub exposed: Vec<Exposed>,
    /// Names of blocks the user edited (or `*`: the file cannot be read or its markers do not
    /// parse).
    pub conflicts: Vec<String>,
    /// Why the file cannot be read or was not written last time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub name: String,
    pub reason: String,
    /// The fragment sha the edited block was written from.
    pub block_fragment: Option<Sha256Hex>,
    /// sha256 of the edited block's inner text (LF) as the file holds it now.
    pub held_sha: Option<Sha256Hex>,
    /// The fragment sha rematch would write now (`None`: it would remove the block).
    pub desired_sha: Option<Sha256Hex>,
}

/// What [`sync`] did (or, dry, would do) to the personal context file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sync {
    pub target: PathBuf,
    pub applied: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// Approved texts left out by the budget.
    #[serde(default)]
    pub over_budget: Vec<String>,
    pub changed: bool,
    pub created: bool,
    pub deleted: bool,
    pub dry_run: bool,
}

impl Sync {
    /// Human lines (empty when nothing changed and nothing conflicts).
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let verb = if self.dry_run { "would " } else { "" };
        let mut parts = Vec::new();
        if !self.applied.is_empty() {
            parts.push(format!("{verb}add {}", self.applied.join(", ")));
        }
        if !self.updated.is_empty() {
            parts.push(format!("{verb}update {}", self.updated.join(", ")));
        }
        if !self.removed.is_empty() {
            parts.push(format!("{verb}remove {}", self.removed.join(", ")));
        }
        if !parts.is_empty() {
            out.push(format!(
                "personal AGENTS.md {}: {}; new sessions load it, a running session keeps the text it was already sent (start a new session to apply)",
                self.target.display(),
                parts.join("; ")
            ));
        }
        if !self.over_budget.is_empty() {
            out.push(format!(
                "personal AGENTS.md {}: over the always-on budget, not added: {} (`rematch status` lists them)",
                self.target.display(),
                self.over_budget.join(", ")
            ));
        }
        for c in &self.conflicts {
            out.push(conflict_line(&self.target, c));
        }
        out
    }
}

pub fn conflict_line(target: &Path, c: &Conflict) -> String {
    if c.name == "*" {
        return format!("personal AGENTS.md {}: {}; left untouched", target.display(), c.reason);
    }
    format!(
        "personal AGENTS.md {}: block {} {}; left as is (`rematch library inspect instructions-conflict:{}` to keep your text or {})",
        target.display(),
        c.name,
        c.reason,
        c.name,
        if c.desired_sha.is_some() { "restore the approved text" } else { "remove it" },
    )
}

// ---------------------------------------------------------------------------------------------
// Records

/// Every instruction decision recorded in this ledger about this agent dir's personal file,
/// oldest first. `REMATCH_ROOT` (and its ledger) is shared by every omp profile: a decision about
/// another profile's file never decides this one (the profile carries texts between them only
/// through `profile sync`).
pub fn records(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<InstructionRecord>> {
    let here = target(paths);
    let mut out = Vec::new();
    for row in ledger.list_receipts(None)? {
        if row.receipt.activation != Activation::Prompted {
            continue;
        }
        let Some(bytes) = ledger.get_blob(&row.receipt.certificate).ok().flatten() else { continue };
        let Ok(r) = serde_json::from_slice::<InstructionRecord>(&bytes) else { continue };
        if r.what == RECORD_WHAT && r.target == here {
            out.push(r);
        }
    }
    Ok(out)
}

/// The latest decision about `(name, sha)`.
pub fn decision(records: &[InstructionRecord], name: &str, sha: &str) -> Option<Decision> {
    records.iter().rev().find(|r| r.name == name && r.fragment_sha256 == sha).map(|r| r.decision)
}

fn record(ledger: &Ledger, r: InstructionRecord) -> Result<Receipt> {
    let receipt_entry = r.entry.clone().unwrap_or_else(|| format!("{PROFILE_ENTRY_PREFIX}{}", r.name));
    let cert = ledger.put_blob(serde_json::to_string(&r)?.as_bytes())?;
    let receipt = Receipt { certificate: cert, entry: receipt_entry, activation: Activation::Prompted };
    ledger.insert_receipt(&receipt)?;
    Ok(receipt)
}

fn new_record(paths: &RematchPaths, decision: Decision, entry: Option<EntryId>, name: &str, version: Option<u32>, sha: &str) -> InstructionRecord {
    new_record_with(paths, decision, entry, name, version, sha, None)
}

fn new_record_with(
    paths: &RematchPaths,
    decision: Decision,
    entry: Option<EntryId>,
    name: &str,
    version: Option<u32>,
    sha: &str,
    approver: Option<String>,
) -> InstructionRecord {
    InstructionRecord {
        what: RECORD_WHAT.into(),
        decision,
        entry,
        name: name.to_string(),
        version,
        fragment_sha256: sha.to_string(),
        target: target(paths),
        at: now_ms(),
        approver,
    }
}

// ---------------------------------------------------------------------------------------------
// Entries and fragments

/// An entry whose fragment can reach the personal file: loadable status.
pub fn loadable(e: &Entry) -> bool {
    matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned)
}

/// rematch owns the entry's files ([`library::owns_dir`]); lessons are library records.
pub fn owned(paths: &RematchPaths, e: &Entry) -> bool {
    e.kind != Kind::Lesson && library::owns_dir(paths, e)
}

/// The entry's fragment: `(sha256 of the bytes, text)`; `Err` text when it cannot be applied.
pub fn fragment(e: &Entry) -> Option<(Sha256Hex, std::result::Result<String, String>)> {
    let bytes = std::fs::read(e.path.join(AGENTS_FRAGMENT)).ok()?;
    let sha = sha256_hex(&bytes);
    let text = match String::from_utf8(bytes) {
        Err(_) => Err("AGENTS.fragment.md is not UTF-8".to_string()),
        Ok(t) => check_text(&t).map(|()| t),
    };
    Some((sha, text))
}

/// Text rematch can place between its markers.
fn check_text(t: &str) -> std::result::Result<(), String> {
    if t.trim().is_empty() {
        return Err("AGENTS.fragment.md is empty".into());
    }
    if t.contains(MARKER_PREFIX) {
        return Err("AGENTS.fragment.md holds `<!-- rematch:`, which only rematch's own marker lines may".into());
    }
    Ok(())
}

/// A fragment of a loadable entry awaiting the user's decision.
#[derive(Debug, Clone)]
pub struct Pending {
    pub entry: Entry,
    pub fragment_sha256: Sha256Hex,
    pub text: String,
    /// Why it cannot be approved as is (bad text, or over the budget).
    pub blocked: Option<String>,
    /// The text applied for this name now (what the approval replaces).
    pub applied: Option<Desired>,
    pub target: PathBuf,
    /// Bytes of the rendered block and the budget it must fit.
    pub bytes: usize,
    pub budget: usize,
}

/// No decision about `(name, sha)` stands here: none recorded, or approved here and withdrawn
/// profile-wide since.
fn undecided(recs: &[InstructionRecord], pd: &ProfileDecisions, name: &str, sha: &str) -> bool {
    match latest(recs, name, sha) {
        None => true,
        Some(r) => r.decision == Decision::Approved && withdrawn_since(pd, name, sha, r.at),
    }
}

/// The latest record about `(name, sha)`.
fn latest<'a>(recs: &'a [InstructionRecord], name: &str, sha: &str) -> Option<&'a InstructionRecord> {
    recs.iter().rev().find(|r| r.name == name && r.fragment_sha256 == sha)
}

/// Fragments of loadable rematch-owned entries without a decision for their exact bytes (or
/// approved here and withdrawn profile-wide since), and not already applied from the profile in
/// those bytes. Found from the files on every call: an existing staged fragment is pending until
/// decided.
pub fn pending(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<Pending>> {
    let recs = records(ledger, paths)?;
    let now = desired(ledger, paths, &recs, false, None)?;
    let pd = profile_decisions(paths);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for e in ledger.list_entries(&EntryFilter::default())? {
        if !loadable(&e) || !owned(paths, &e) {
            continue;
        }
        let Some((sha, text)) = fragment(&e) else { continue };
        if !undecided(&recs, &pd, &e.name, &sha) || !seen.insert((e.name.clone(), sha.clone())) {
            continue;
        }
        if now.blocks.get(&e.name).is_some_and(|d| d.source == Source::Profile && d.sha == sha) {
            continue;
        }
        let (text, mut blocked) = match text {
            Ok(t) => (t, None),
            Err(why) => (String::from_utf8_lossy(&std::fs::read(e.path.join(AGENTS_FRAGMENT)).unwrap_or_default()).into_owned(), Some(why)),
        };
        if blocked.is_none() {
            blocked = validate_name(&e.name).err().map(|x| format!("{x:#}"));
        }
        let candidate = Desired { name: e.name.clone(), sha: sha.clone(), text: text.clone(), entry: Some(e.id.clone()), source: Source::Ledger, approver: None };
        let bytes = candidate.bytes();
        if blocked.is_none() {
            blocked = over_budget(ledger, paths, &recs, &e, &sha)?;
        }
        if blocked.is_none() {
            blocked = check_room(ledger, paths, &e, bytes).err().map(|x| format!("{x:#}"));
        }
        out.push(Pending {
            applied: now.blocks.get(&e.name).cloned(),
            entry: e,
            fragment_sha256: sha,
            text,
            blocked,
            target: target(paths),
            bytes,
            budget: now.budget,
        });
    }
    Ok(out)
}

/// Why approving `sha` for `e` would not fit the budget (`None`: it fits).
fn over_budget(ledger: &Ledger, paths: &RematchPaths, recs: &[InstructionRecord], e: &Entry, sha: &str) -> Result<Option<String>> {
    let extra = new_record(paths, Decision::Approved, Some(e.id.clone()), &e.name, Some(e.version), sha);
    let set = desired(ledger, paths, recs, false, Some((&extra, fragment(e).and_then(|(_, t)| t.ok()).unwrap_or_default())))?;
    let Some(o) = set.over.iter().find(|o| o.name == e.name && o.sha == sha) else { return Ok(None) };
    let others: Vec<String> = set.blocks.values().filter(|b| b.name != e.name).map(|b| format!("{} {} B", b.name, b.bytes())).collect();
    Ok(Some(format!(
        "over the personal-instructions budget: this block {} B + {} B already applied > {} B (`limits.always_on_bytes` of {}); \
         withdraw one of [{}] (`rematch library inspect instructions-applied:<name>`) or shorten the fragment",
        o.bytes(),
        set.used,
        set.budget,
        paths.improver.join("lint.toml").display(),
        others.join(", ")
    )))
}

/// The rendered block, with the entry's own catalog line, next to everything else rematch keeps
/// always on where the entry loads (catalog lines, approved agents, runtime fragments, the other
/// blocks: [`library::check_always_on`]). Staging linted the fragment alone; approval is where it
/// becomes always on.
fn check_room(ledger: &Ledger, paths: &RematchPaths, e: &Entry, block: usize) -> Result<()> {
    let cost = crate::lint::entry_cost(&e.path, e.kind).catalog + block;
    library::check_always_on(ledger, paths, e, cost, "personal instructions block + its catalog line")
}

// ---------------------------------------------------------------------------------------------
// Desired state

pub fn profile_dir(paths: &RematchPaths) -> PathBuf {
    crate::profile::instructions_dir(paths)
}

fn profile_texts(paths: &RematchPaths) -> Vec<(String, String)> {
    if !crate::profile::present(paths) || skipped_profile(paths) {
        return Vec::new();
    }
    let mut out: Vec<(String, String)> = std::fs::read_dir(profile_dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| {
            let p = d.path();
            let name = p.file_name()?.to_str()?.strip_suffix(".md")?.to_string();
            validate_name(&name).ok()?;
            let text = std::fs::read_to_string(&p).ok()?;
            check_text(&text).ok()?;
            Some((name, text))
        })
        .collect();
    out.sort();
    out
}

/// `profile/instructions/<name>.decision`: the latest profile-wide decision about the name's
/// text — `approved` by some machine (or omp profile) at `at`, or `declined` there (declined or
/// withdrawn). A withdrawal makes every machine drop those bytes, its own earlier approval
/// included; a later approval anywhere brings them back, a local decline included. The text
/// itself stays `<name>.md` (exact bytes); a profile without decision files is all approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDecision {
    pub sha: Sha256Hex,
    pub decision: Decision,
    pub at: UnixMs,
}

type ProfileDecisions = BTreeMap<String, ProfileDecision>;

const DECISION_EXT: &str = "decision";

fn decision_file(paths: &RematchPaths, name: &str) -> PathBuf {
    profile_dir(paths).join(format!("{name}.{DECISION_EXT}"))
}

/// The profile's decisions by name (none without a profile, or under a skipped omp profile).
fn profile_decisions(paths: &RematchPaths) -> ProfileDecisions {
    if !crate::profile::present(paths) || skipped_profile(paths) {
        return BTreeMap::new();
    }
    std::fs::read_dir(profile_dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|d| {
            let p = d.path();
            let name = p.file_name()?.to_str()?.strip_suffix(&format!(".{DECISION_EXT}"))?.to_string();
            validate_name(&name).ok()?;
            let w: ProfileDecision = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            Some((name, w))
        })
        .collect()
}

/// The profile withdrew `(name, sha)` at or after `at`.
fn withdrawn_since(pd: &ProfileDecisions, name: &str, sha: &str, at: UnixMs) -> bool {
    pd.get(name).is_some_and(|d| d.decision == Decision::Declined && d.sha == sha && d.at >= at)
}

/// The profile approved `(name, sha)` after `at`.
fn approved_after(pd: &ProfileDecisions, name: &str, sha: &str, at: UnixMs) -> bool {
    pd.get(name).is_some_and(|d| d.decision == Decision::Approved && d.sha == sha && d.at > at)
}

/// The active omp profile is one `REMATCH_ROOT/policy.json` `skip_profiles` excludes (as
/// `import --history` does): the profile's texts are not materialized into its agent dir. Texts
/// the user approves there explicitly still apply.
pub fn skipped_profile(paths: &RematchPaths) -> bool {
    #[derive(Deserialize)]
    struct Policy {
        #[serde(default)]
        skip_profiles: Vec<String>,
    }
    let Some(active) = &paths.profile else { return false };
    let Ok(text) = std::fs::read_to_string(paths.root.join(crate::domain::POLICY_FILE)) else { return false };
    serde_json::from_str::<Policy>(&text)
        .map(|p| p.skip_profiles.iter().filter_map(|s| crate::paths::normalize_profile_name(s)).any(|s| &s == active))
        .unwrap_or(false)
}

/// This ledger accounts for the profile's text of `name` in these bytes: the user declined it
/// here (and no machine approved it since), or approved it here and neither the entry it was
/// approved for nor any entry holding these bytes is loadable anymore (the profile copy is an
/// echo the next `profile sync` removes). A loadable entry of the same name with other bytes
/// keeps nothing alive.
fn suppressed(recs: &[InstructionRecord], pd: &ProfileDecisions, live: &[(&Entry, Option<Sha256Hex>)], name: &str, sha: &str) -> bool {
    let Some(last) = latest(recs, name, sha) else { return false };
    match last.decision {
        Decision::Declined => !approved_after(pd, name, sha, last.at),
        Decision::Approved => {
            let held = live.iter().any(|(e, s)| e.name == name && s.as_deref() == Some(sha));
            let own = recs.iter().any(|r| {
                r.name == name
                    && r.fragment_sha256 == sha
                    && r.decision == Decision::Approved
                    && r.entry.as_ref().is_some_and(|id| live.iter().any(|(e, _)| &e.id == id))
            });
            !held && !own
        }
    }
}

/// Loadable rematch-owned entries with the sha of their fragment.
fn live_entries<'a>(paths: &RematchPaths, entries: &'a [Entry]) -> Vec<(&'a Entry, Option<Sha256Hex>)> {
    entries.iter().filter(|e| loadable(e) && owned(paths, e)).map(|e| (e, fragment(e).map(|(s, _)| s))).collect()
}

/// The blocks the file holds now with whether each was edited and its bytes (unreadable or
/// unparseable: none), for budget priority.
fn current_blocks(paths: &RematchPaths) -> Vec<(Block, bool, usize)> {
    let Ok(Some(bytes)) = read_opt(&real_path(&target(paths))) else { return Vec::new() };
    let Ok(text) = String::from_utf8(bytes) else { return Vec::new() };
    let Ok(p) = parse(&text) else { return Vec::new() };
    p.blocks.into_iter().map(|b| (b.edited(&text), b.end - b.start, b)).map(|(e, n, b)| (b, e, n)).collect()
}

/// What the personal file should hold. `respect_off`: nothing while rematch is off. `extra`: a
/// hypothetical approval (with its text) to test whether it would fit.
///
/// Per name, the newest approved text that a loadable, rematch-owned entry of the name holds now
/// — or that was approved for such an entry, whose revised fragment awaits its own approval —
/// unless the newest approved text was withdrawn since (then none: an older one never returns by
/// itself). Another entry of the same name with other bytes keeps nothing alive. Then the
/// profile's texts of names not decided here. Admission to the budget: unedited blocks already in
/// the file in these bytes stay, the rest by name while they fit.
pub fn desired(
    ledger: &Ledger,
    paths: &RematchPaths,
    recs: &[InstructionRecord],
    respect_off: bool,
    extra: Option<(&InstructionRecord, String)>,
) -> Result<DesiredSet> {
    let mut set = DesiredSet { budget: budget(paths), ..Default::default() };
    if respect_off && paths.is_off() {
        return Ok(set);
    }
    let mut recs = recs.to_vec();
    let mut extra_text = None;
    if let Some((r, t)) = extra {
        recs.push(r.clone());
        extra_text = Some((r.fragment_sha256.clone(), t));
    }
    let entries = ledger.list_entries(&EntryFilter::default())?;
    let live = live_entries(paths, &entries);
    let names: BTreeSet<&str> = live.iter().map(|(e, _)| e.name.as_str()).collect();
    let pd = profile_decisions(paths);
    let mut all: BTreeMap<String, Desired> = BTreeMap::new();
    for name in names {
        if validate_name(name).is_err() {
            continue;
        }
        let approved = recs.iter().rev().filter(|r| r.name == name && r.decision == Decision::Approved);
        for r in approved {
            // The newest approved text was withdrawn since, here or profile-wide: the name holds
            // nothing from this ledger (an older approved text never comes back by itself).
            let elsewhere = withdrawn_since(&pd, name, &r.fragment_sha256, r.at);
            if elsewhere || decision(&recs, name, &r.fragment_sha256) != Some(Decision::Approved) {
                break;
            }
            let holder = live.iter().find(|(e, s)| e.name == name && s.as_deref() == Some(r.fragment_sha256.as_str())).map(|(e, _)| e.id.clone());
            let own = r.entry.as_ref().filter(|id| live.iter().any(|(e, _)| &e.id == *id)).cloned();
            let Some(credit) = holder.or(own) else { continue };
            let text = match &extra_text {
                Some((s, t)) if *s == r.fragment_sha256 => Some(t.clone()),
                _ => ledger.get_blob(&r.fragment_sha256)?.and_then(|b| String::from_utf8(b).ok()),
            };
            let Some(text) = text.filter(|t| check_text(t).is_ok()) else { continue };
            all.insert(name.to_string(), Desired { name: name.to_string(), sha: r.fragment_sha256.clone(), text, entry: Some(credit), source: Source::Ledger, approver: r.approver.clone() });
            break;
        }
    }
    for (name, text) in profile_texts(paths) {
        if all.contains_key(&name) {
            continue;
        }
        let sha = sha256_hex(text.as_bytes());
        if suppressed(&recs, &pd, &live, &name, &sha) || withdrawn_since(&pd, &name, &sha, 0) {
            continue;
        }
        let credited = live.iter().find(|(e, s)| e.name == name && s.as_deref() == Some(sha.as_str())).map(|(e, _)| e.id.clone());
        all.insert(name.clone(), Desired { name, sha, text, entry: credited, source: Source::Profile, approver: None });
    }
    // Budget: edited blocks stay (the user's bytes), and so do unedited ones in the desired bytes
    // (approved and admitted before; a lowered limit never takes them out silently). The rest
    // are admitted by name while they fit; what does not waits as a blocked action.
    let current = current_blocks(paths);
    for (b, edited, n) in &current {
        if *edited {
            if !all.contains_key(&b.name) {
                set.used += n;
            }
            continue;
        }
        if let Some(d) = all.remove(&b.name) {
            if d.sha == b.fragment {
                set.used += d.bytes();
                set.blocks.insert(b.name.clone(), d);
            } else {
                all.insert(b.name.clone(), d);
            }
        }
    }
    for (name, d) in all {
        let n = d.bytes();
        if set.used + n <= set.budget {
            set.used += n;
            set.blocks.insert(name, d);
        } else {
            set.over.push(d);
        }
    }
    Ok(set)
}

// ---------------------------------------------------------------------------------------------
// The file

#[derive(Debug, Clone, PartialEq, Eq)]
struct Block {
    name: String,
    sha: String,
    fragment: String,
    pad: usize,
    /// Byte offset of the begin line.
    start: usize,
    /// Byte offset just past the end line.
    end: usize,
    inner_start: usize,
    inner_end: usize,
}

impl Block {
    fn inner<'a>(&self, text: &'a str) -> &'a str {
        &text[self.inner_start..self.inner_end]
    }

    fn edited(&self, text: &str) -> bool {
        inner_sha(self.inner(text)) != self.sha
    }
}

/// sha256 of a block's inner text in LF form.
pub fn inner_sha(inner: &str) -> String {
    sha256_hex(inner.replace("\r\n", "\n").as_bytes())
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn attrs(line: &str, prefix: &str) -> Option<BTreeMap<String, String>> {
    let rest = line.trim_end_matches(['\r', '\n']).strip_prefix(prefix)?.strip_suffix(CLOSE)?;
    Some(rest.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

/// A begin marker rematch writes: `(name, inner sha, fragment sha, pad)`.
fn begin_marker(line: &str) -> Option<(String, String, String, usize)> {
    let a = attrs(line, BEGIN)?;
    let name = a.get("name")?;
    let (sha, fragment) = (a.get("sha256")?, a.get("fragment")?);
    if validate_name(name).is_err() || !is_sha(sha) || !is_sha(fragment) {
        return None;
    }
    let pad = a.get("pad").and_then(|p| p.parse().ok()).unwrap_or(0).min(2);
    Some((name.clone(), sha.clone(), fragment.clone(), pad))
}

/// The name of an end marker rematch writes.
fn end_marker(line: &str) -> Option<String> {
    let a = attrs(line, END)?;
    let name = a.get("name")?;
    validate_name(name).ok()?;
    Some(name.clone())
}

/// A code fence opener/closer (```` ``` ```` or `~~~`, up to three spaces of indent).
fn fence(line: &str) -> Option<char> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    if t.starts_with("```") {
        Some('`')
    } else if t.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

struct Parsed {
    blocks: Vec<Block>,
    /// The text after the last block ends inside an open code fence.
    open_fence: bool,
}

/// The managed blocks of `text`, or why its markers do not parse. Marker lines inside code
/// fences outside blocks, and malformed ones, are the user's text.
fn parse(text: &str) -> std::result::Result<Parsed, String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut open: Option<Block> = None;
    let mut in_fence: Option<char> = None;
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        let start = off;
        off += line.len();
        if open.is_none() {
            if let Some(f) = fence(line) {
                in_fence = match in_fence {
                    None => Some(f),
                    Some(c) if c == f => None,
                    other => other,
                };
                continue;
            }
            if in_fence.is_some() {
                continue;
            }
        }
        if let Some((name, sha, fragment, pad)) = begin_marker(line) {
            if let Some(b) = &open {
                return Err(format!("block {} has no end marker before the next begin marker", b.name));
            }
            if blocks.iter().any(|b| b.name == name) {
                return Err(format!("block {name} appears twice"));
            }
            open = Some(Block { name, sha, fragment, pad, start, end: 0, inner_start: off, inner_end: 0 });
        } else if let Some(name) = end_marker(line) {
            let Some(mut b) = open.take() else { return Err(format!("the end marker of block {name} has no begin marker")) };
            if name != b.name {
                return Err(format!("block {} ends with the end marker of {name}", b.name));
            }
            b.inner_end = start;
            b.end = off;
            blocks.push(b);
        }
    }
    if let Some(b) = open {
        return Err(format!("block {} has no end marker", b.name));
    }
    Ok(Parsed { blocks, open_fence: in_fence.is_some() })
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// The lines between the markers, LF form: what an approval of `text` writes for `name`.
pub fn render_inner(name: &str, text: &str) -> String {
    let body = text.replace("\r\n", "\n");
    format!("## rematch: {name}\n\n{}\n", body.trim_end())
}

fn render_block(d: &Desired, pad: usize, eol: &str) -> String {
    let inner = render_inner(&d.name, &d.text);
    let sha = sha256_hex(inner.as_bytes());
    format!(
        "{BEGIN}name={} sha256={sha} fragment={} pad={pad}{CLOSE}{eol}{}{END}name={}{CLOSE}{eol}",
        d.name,
        d.sha,
        inner.replace('\n', eol),
        d.name
    )
}

/// What to do with blocks the user edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edited {
    Keep,
    /// Overwrite (restore) — an explicit user decision for the named blocks.
    Overwrite,
    /// Drop the markers, leave the inner text in place as the user's own.
    Release,
}

/// The new file text for `desired`, leaving everything outside managed blocks as it is.
fn plan(cur: &str, desired: &BTreeMap<String, Desired>, edited: &BTreeMap<String, Edited>, out: &mut Sync) -> std::result::Result<String, String> {
    let parsed = parse(cur)?;
    let eol = eol_of(cur);
    let mut text = String::with_capacity(cur.len() + 256);
    let mut pos = 0usize;
    let mut present = BTreeSet::new();
    for b in &parsed.blocks {
        present.insert(b.name.clone());
        let want = desired.get(&b.name);
        let is_edited = b.edited(cur);
        let how = if is_edited { edited.get(&b.name).copied().unwrap_or(Edited::Keep) } else { Edited::Overwrite };
        let same = want.is_some_and(|d| d.sha == b.fragment && !is_edited && render_inner(&d.name, &d.text) == b.inner(cur).replace("\r\n", "\n"));
        if same {
            continue;
        }
        match how {
            Edited::Keep => {
                out.conflicts.push(Conflict {
                    name: b.name.clone(),
                    reason: "was edited by hand".into(),
                    block_fragment: Some(b.fragment.clone()),
                    held_sha: Some(inner_sha(b.inner(cur))),
                    desired_sha: want.map(|d| d.sha.clone()),
                });
                continue;
            }
            Edited::Release => {
                text.push_str(&cur[pos..b.start]);
                text.push_str(b.inner(cur));
                pos = b.end;
                out.removed.push(format!("{} (markers only; text kept as yours)", b.name));
                continue;
            }
            Edited::Overwrite => {}
        }
        match want {
            Some(d) => {
                text.push_str(&cur[pos..b.start]);
                text.push_str(&render_block(d, b.pad, eol));
                out.updated.push(b.name.clone());
            }
            None => {
                // Drop the line breaks rematch added before the block when they are still there
                // and belong to the text since the previous cut.
                let mut cut = b.start;
                for nl in ["\r\n", "\n"] {
                    let n = nl.len() * b.pad;
                    if b.pad > 0 && b.start >= pos + n && cur[b.start - n..b.start] == nl.repeat(b.pad) {
                        cut = b.start - n;
                        break;
                    }
                }
                text.push_str(&cur[pos..cut]);
                out.removed.push(b.name.clone());
            }
        }
        pos = b.end;
    }
    text.push_str(&cur[pos..]);
    let adds: Vec<&Desired> = desired.values().filter(|d| !present.contains(&d.name)).collect();
    if !adds.is_empty() && parsed.open_fence {
        return Err("the file ends inside an open code fence; close it so rematch can append its blocks".into());
    }
    for d in adds {
        let pad = if text.is_empty() || text.ends_with(&format!("{eol}{eol}")) || text == eol {
            0
        } else if text.ends_with('\n') {
            1
        } else {
            2
        };
        text.push_str(&eol.repeat(pad));
        text.push_str(&render_block(d, pad, eol));
        out.applied.push(d.name.clone());
    }
    Ok(text)
}

/// The file the target names: a symlinked `AGENTS.md` (dotfiles) is written through.
fn real_path(p: &Path) -> PathBuf {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => crate::paths::canonicalize(p).unwrap_or_else(|_| {
            let t = std::fs::read_link(p).unwrap_or_default();
            p.parent().map(|d| d.join(&t)).unwrap_or(t)
        }),
        _ => p.to_path_buf(),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// The personal file this state is about.
    #[serde(default)]
    target: Option<PathBuf>,
    #[serde(default)]
    created: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error_at: Option<UnixMs>,
}

fn state_path(paths: &RematchPaths) -> PathBuf {
    let key = sha256_hex(target(paths).to_string_lossy().as_bytes());
    paths.root.join(STATE_DIR).join(format!("{}.json", &key[..16]))
}

fn load_state(paths: &RematchPaths) -> State {
    let here = target(paths);
    std::fs::read_to_string(state_path(paths))
        .ok()
        .and_then(|t| serde_json::from_str::<State>(&t).ok())
        .filter(|s| s.target.as_deref() == Some(here.as_path()))
        .unwrap_or(State { target: Some(here), ..Default::default() })
}

fn save_state(paths: &RematchPaths, s: &State) -> Result<()> {
    let p = state_path(paths);
    std::fs::create_dir_all(p.parent().expect("under the root"))?;
    std::fs::write(&p, serde_json::to_string(s)?)?;
    // The one root-wide state of earlier builds named no file: whichever omp profile wrote it,
    // it is never trusted (a `created` from another agent dir must not delete this one) and goes.
    let _ = std::fs::remove_file(paths.root.join(LEGACY_STATE_FILE));
    Ok(())
}

/// The last failed write of the personal file, while no later write succeeded.
pub fn last_error(paths: &RematchPaths) -> Option<String> {
    load_state(paths).last_error
}

fn read_opt(p: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// The exclusive OS lock of the writers (`flock` / `LockFileEx` through std); released on drop
/// or when the holder dies.
struct WriteLock(std::fs::File);

impl WriteLock {
    fn take(paths: &RematchPaths) -> Result<WriteLock> {
        std::fs::create_dir_all(&paths.root)?;
        let p = paths.root.join(LOCK_FILE);
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).with_context(|| format!("lock {}", p.display()))?;
        f.lock().with_context(|| format!("lock {}", p.display()))?;
        Ok(WriteLock(f))
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// A temporary file next to the target, removed on drop unless renamed into place.
struct Temp(Option<PathBuf>);

impl Temp {
    fn new(dir: &Path) -> Temp {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        Temp(Some(dir.join(format!(".{AGENTS_MD}.rematch-{}-{nanos}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("live temp")
    }

    fn persist(mut self, to: &Path) -> Result<()> {
        let p = self.0.take().expect("live temp");
        std::fs::rename(&p, to).with_context(|| format!("replace {}", to.display())).inspect_err(|_| {
            let _ = std::fs::remove_file(&p);
        })
    }

    /// Remove temporaries a writer killed between write and rename left behind (older than a
    /// minute: a live writer of another root renames within that).
    fn sweep(dir: &Path) {
        let prefix = format!(".{AGENTS_MD}.rematch-");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        for d in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = d.file_name();
            let Some(n) = name.to_str() else { continue };
            if n.starts_with(&prefix) && n.ends_with(".tmp") && d.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < old) {
                let _ = std::fs::remove_file(d.path());
            }
        }
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A reviewed block: the edited block `name` must still hold this inner text.
struct Expect<'a> {
    name: &'a str,
    held_sha: &'a str,
}

/// The reviewed edited block changed before the write: refused, nothing written.
#[derive(Debug)]
struct Reviewed(String);

impl std::fmt::Display for Reviewed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "block {} changed since it was shown; review it again: `rematch library inspect instructions-conflict:{}`", self.0, self.0)
    }
}

impl std::error::Error for Reviewed {}

/// Plan (and unless `dry_run`, write) the file for `desired`. Writers hold the OS lock; the file
/// is re-read just before the rename and the plan recomputed when someone else wrote it
/// meanwhile. Read and parse failures of a dry run are conflicts (`*`); of a write, errors, which
/// are recorded as `last_error`.
fn apply(paths: &RematchPaths, set: &DesiredSet, edited: &BTreeMap<String, Edited>, expect: Option<Expect>, dry_run: bool) -> Result<Sync> {
    let shown = target(paths);
    let file = real_path(&shown);
    let over: Vec<String> = set.over.iter().map(|d| d.name.clone()).collect();
    if dry_run {
        let mut out = Sync { target: shown.clone(), dry_run, over_budget: over, ..Default::default() };
        let cur = match read_opt(&file) {
            Ok(None) => String::new(),
            Ok(Some(b)) => match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => {
                    out.conflicts.push(Conflict { name: "*".into(), reason: "the file is not UTF-8".into(), ..Default::default() });
                    return Ok(out);
                }
            },
            Err(e) => {
                out.conflicts.push(Conflict { name: "*".into(), reason: format!("cannot read it: {e:#}"), ..Default::default() });
                return Ok(out);
            }
        };
        match plan(&cur, &set.blocks, edited, &mut out) {
            Ok(text) => out.changed = text != cur && !(cur.is_empty() && text.is_empty()),
            Err(why) => out.conflicts.push(Conflict { name: "*".into(), reason: why, ..Default::default() }),
        }
        return Ok(out);
    }
    let _lock = WriteLock::take(paths)?;
    let res = write_locked(paths, &shown, &file, set, edited, expect, over);
    let mut state = load_state(paths);
    match &res {
        // A reviewed block that changed since is the user's race, not a failed write.
        Err(e) if e.downcast_ref::<Reviewed>().is_some() => {}
        Err(e) => {
            state.last_error = Some(format!("{e:#}"));
            state.last_error_at = Some(now_ms());
            let _ = save_state(paths, &state);
        }
        Ok(_) if state.last_error.is_some() => {
            state.last_error = None;
            state.last_error_at = None;
            save_state(paths, &state)?;
        }
        Ok(_) => {}
    }
    res
}

fn write_locked(
    paths: &RematchPaths,
    shown: &Path,
    file: &Path,
    set: &DesiredSet,
    edited: &BTreeMap<String, Edited>,
    expect: Option<Expect>,
    over: Vec<String>,
) -> Result<Sync> {
    for _ in 0..3 {
        let mut out = Sync { target: shown.to_path_buf(), over_budget: over.clone(), ..Default::default() };
        let before = read_opt(file)?;
        let cur = match &before {
            None => String::new(),
            Some(b) => match std::str::from_utf8(b) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    out.conflicts.push(Conflict { name: "*".into(), reason: "the file is not UTF-8".into(), ..Default::default() });
                    return Ok(out);
                }
            },
        };
        if let Some(x) = &expect {
            let held = parse(&cur).ok().and_then(|p| p.blocks.into_iter().find(|b| b.name == x.name)).map(|b| inner_sha(b.inner(&cur)));
            if held.as_deref() != Some(x.held_sha) {
                return Err(Reviewed(x.name.to_string()).into());
            }
        }
        let text = match plan(&cur, &set.blocks, edited, &mut out) {
            Ok(t) => t,
            Err(why) => {
                out.conflicts.push(Conflict { name: "*".into(), reason: why, ..Default::default() });
                return Ok(out);
            }
        };
        if before.is_none() && text.is_empty() || text == cur {
            return Ok(out);
        }
        out.changed = true;
        let mut state = load_state(paths);
        let delete = text.is_empty() && state.created;
        out.created = before.is_none();
        out.deleted = delete;
        let dir = file.parent().ok_or_else(|| anyhow!("{} has no parent", file.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        Temp::sweep(dir);
        let tmp = Temp::new(dir);
        if !delete {
            std::fs::write(tmp.path(), text.as_bytes()).with_context(|| format!("write {}", tmp.path().display()))?;
        }
        if read_opt(file)? != before {
            continue;
        }
        if delete {
            std::fs::remove_file(file).with_context(|| format!("remove {}", file.display()))?;
            state.created = false;
        } else {
            tmp.persist(file)?;
            if out.created {
                state.created = true;
            }
        }
        save_state(paths, &state)?;
        return Ok(out);
    }
    bail!("{} kept changing while rematch updated it; run the command again", file.display())
}

// ---------------------------------------------------------------------------------------------
// Public operations

/// Bring the personal file in line with the approvals, the profile, the budget and the off
/// switch. Idempotent; edited blocks are reported, never touched.
pub fn sync(ledger: &Ledger, paths: &RematchPaths) -> Result<Sync> {
    let recs = records(ledger, paths)?;
    let set = desired(ledger, paths, &recs, true, None)?;
    apply(paths, &set, &BTreeMap::new(), None, false)
}

/// [`sync`] without writing: what it would do (`changed`: the file is not what it should be),
/// conflicts and texts over the budget included. Never fails on the file.
pub fn plan_sync(ledger: &Ledger, paths: &RematchPaths) -> Result<Sync> {
    let recs = records(ledger, paths)?;
    let set = desired(ledger, paths, &recs, true, None)?;
    apply(paths, &set, &BTreeMap::new(), None, true)
}

/// The lines between the markers of block `name` as the file holds them now (edited or not).
pub fn block_inner(paths: &RematchPaths, name: &str) -> Option<String> {
    let text = String::from_utf8(read_opt(&real_path(&target(paths))).ok()??).ok()?;
    let b = parse(&text).ok()?.blocks.into_iter().find(|b| b.name == name)?;
    Some(b.inner(&text).replace("\r\n", "\n"))
}

/// Remove every unchanged rematch block (uninstall); `dry_run` plans only. Edited blocks stay
/// and are reported.
pub fn uninstall(paths: &RematchPaths, dry_run: bool) -> Result<Sync> {
    apply(paths, &DesiredSet::default(), &BTreeMap::new(), None, dry_run)
}

/// What omp loads from the personal file now: the unedited blocks, credited to the desired
/// entry of their name when the text is the desired one (else `source: stale`). Never fails on
/// the file: an unreadable one is the conflict `*` with `error`.
pub fn exposure(ledger: &Ledger, paths: &RematchPaths) -> Result<Exposure> {
    let shown = target(paths);
    let mut out = Exposure { host: HOST.into(), target: shown.clone(), error: last_error(paths), ..Default::default() };
    let bytes = match read_opt(&real_path(&shown)) {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(out),
        Err(e) => {
            out.conflicts.push("*".into());
            out.error = Some(format!("{e:#}"));
            return Ok(out);
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        out.conflicts.push("*".into());
        return Ok(out);
    };
    let Ok(parsed) = parse(&text) else {
        out.conflicts.push("*".into());
        return Ok(out);
    };
    let recs = records(ledger, paths)?;
    let d = desired(ledger, paths, &recs, false, None)?;
    for b in parsed.blocks {
        if b.edited(&text) {
            out.conflicts.push(b.name);
            continue;
        }
        let want = d.blocks.get(&b.name).filter(|w| w.sha == b.fragment);
        out.exposed.push(Exposed {
            id: want.and_then(|w| w.entry.clone()),
            sha: b.fragment.clone(),
            source: match want.map(|w| w.source) {
                Some(Source::Ledger) => "ledger".into(),
                Some(Source::Profile) => "profile".into(),
                None => "stale".into(),
            },
            bytes: b.end - b.start,
            name: b.name,
        });
    }
    Ok(out)
}

/// Approve the exact fragment of a loadable entry. `sha` is the fragment hash (≥ 8 hex) the
/// user reviewed; a fragment that changed since, or that would not fit the budget, is refused.
pub fn approve(ledger: &Ledger, paths: &RematchPaths, entry: &str, sha: &str) -> Result<(Receipt, Sync)> {
    approve_with(ledger, paths, entry, sha, None)
}

/// [`approve`] recording `approver` (e.g. `"rule:instructions:**"`) in the receipt certificate.
pub fn approve_with(ledger: &Ledger, paths: &RematchPaths, entry: &str, sha: &str, approver: Option<&str>) -> Result<(Receipt, Sync)> {
    let e = ledger.find_entry(entry)?.ok_or_else(|| anyhow!("no entry {entry:?}"))?;
    if !loadable(&e) {
        bail!("entry {} is {:?}; only provisional, kept or pinned entries contribute personal instructions", e.id, e.status);
    }
    if !owned(paths, &e) {
        bail!("{} is not a rematch-owned entry; its files are yours", e.name);
    }
    validate_name(&e.name)?;
    let (cur, text) = fragment(&e).ok_or_else(|| anyhow!("entry {} has no {AGENTS_FRAGMENT}", e.name))?;
    let text = text.map_err(|why| anyhow!("{why}"))?;
    check_sha(&cur, sha, &e.id)?;
    let recs = records(ledger, paths)?;
    if let Some(why) = over_budget(ledger, paths, &recs, &e, &cur)? {
        bail!("{why}");
    }
    let block = Desired { name: e.name.clone(), sha: cur.clone(), text: text.clone(), entry: Some(e.id.clone()), source: Source::Ledger, approver: approver.map(str::to_string) };
    check_room(ledger, paths, &e, block.bytes())?;
    ledger.put_blob(text.as_bytes())?;
    // The same text reaches the system prompt of matched main sessions on hosts without a
    // personal file rematch manages (Claude Code, Pi, OpenCode: `library::prompt_fragments`).
    if !library::fragment_approved(&e) {
        library::approve_with(ledger, paths, &e.id, Approval::Prompt, approver, None)?;
    }
    let receipt = record(ledger, new_record_with(paths, Decision::Approved, Some(e.id.clone()), &e.name, Some(e.version), &cur, approver.map(str::to_string)))?;
    Ok((receipt, sync_after(ledger, paths, "the approval")?))
}

/// Apply standing `policy.json` `auto` rules for `instructions` to undecided fragments of
/// loadable rematch-owned entries that fit within the always-on byte budget.
pub fn apply_auto_rules(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<(Entry, Receipt)>> {
    let rules = crate::domain::load_auto_rules(paths)?;
    if !rules.iter().any(|r| r.surface == crate::domain::AutoSurface::Instructions) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for p in pending(ledger, paths)? {
        if p.blocked.is_some() {
            continue;
        }
        let Some(rule) = crate::domain::matching_auto_rule(&rules, crate::domain::AutoSurface::Instructions, &p.entry.predicate) else {
            continue;
        };
        let approver = format!("rule:{}:{}", rule.surface.as_str(), rule.scope);
        if let Ok((receipt, _)) = approve_with(ledger, paths, &p.entry.id, &p.fragment_sha256, Some(&approver)) {
            out.push((p.entry, receipt));
        }
    }
    Ok(out)
}

/// [`sync`] after a recorded decision: a failed write says the decision stands and how the drift
/// stays visible, rather than looking like the decision failed.
fn sync_after(ledger: &Ledger, paths: &RematchPaths, what: &str) -> Result<Sync> {
    sync(ledger, paths).with_context(|| {
        format!(
            "{what} is recorded, but {} was not updated (listed as `instructions-sync` in `rematch status` until a write succeeds)",
            target(paths).display()
        )
    })
}

fn check_sha(cur: &str, sha: &str, id: &str) -> Result<()> {
    let s = sha.trim().to_ascii_lowercase();
    if s.len() < 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the fragment hash shown with the action is needed (at least 8 hex digits)");
    }
    if !cur.starts_with(&s) {
        bail!(
            "the fragment changed since it was shown (now {}); review it again: `rematch library inspect instructions:{id}`",
            &cur[..12]
        );
    }
    Ok(())
}

/// What a decline did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Declined {
    pub name: String,
    pub fragment_sha256: Sha256Hex,
    /// `pending` (a text not taken) or `applied` (the applied text withdrawn).
    pub what: &'static str,
    pub receipt: Receipt,
    pub sync: Sync,
}

/// Decline the pending fragment of an entry, bound to the fragment hash the user reviewed. Its
/// skill and any approved text of the name stay. Never retires the entry.
pub fn decline(ledger: &Ledger, paths: &RematchPaths, entry: &str, sha: &str) -> Result<Declined> {
    let e = ledger.find_entry(entry)?.ok_or_else(|| anyhow!("no entry {entry:?}"))?;
    let (cur, _) = fragment(&e).ok_or_else(|| anyhow!("entry {} has no {AGENTS_FRAGMENT}", e.name))?;
    check_sha(&cur, sha, &e.id)?;
    if !undecided(&records(ledger, paths)?, &profile_decisions(paths), &e.name, &cur) {
        bail!("the text of {} ({}) was already decided", e.name, &cur[..12]);
    }
    let receipt = record(ledger, new_record(paths, Decision::Declined, Some(e.id.clone()), &e.name, Some(e.version), &cur))?;
    Ok(Declined { name: e.name.clone(), fragment_sha256: cur, what: "pending", receipt, sync: sync_after(ledger, paths, "the decline")? })
}

/// Withdraw the text applied (or due, or over the budget) for `name`, bound to its fragment
/// hash. A profile-sourced or profile-synced text also leaves `profile/instructions/` at the next
/// `profile sync`, so every synced machine drops it. A later approval of the same bytes
/// re-applies it.
pub fn withdraw(ledger: &Ledger, paths: &RematchPaths, name: &str, sha: &str) -> Result<Declined> {
    let recs = records(ledger, paths)?;
    let set = desired(ledger, paths, &recs, false, None)?;
    let w = set
        .blocks
        .get(name)
        .or_else(|| set.over.iter().find(|o| o.name == name))
        .ok_or_else(|| anyhow!("no personal instructions applied for {name:?}"))?;
    if !w.sha.starts_with(&sha.trim().to_ascii_lowercase()) || sha.trim().len() < 8 {
        bail!("the applied text of {name} is now {}; review it again: `rematch library inspect instructions-applied:{name}`", &w.sha[..12]);
    }
    let receipt = record(ledger, new_record(paths, Decision::Declined, w.entry.clone(), name, None, &w.sha))?;
    Ok(Declined { name: name.to_string(), fragment_sha256: w.sha.clone(), what: "applied", receipt, sync: sync_after(ledger, paths, "the withdrawal")? })
}

/// How the user resolves an edited block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    /// Overwrite the edited block with the approved text (or remove it when nothing is approved).
    Restore,
    /// Keep the edited text as the user's own: drop the markers and withdraw the approved text so
    /// it is not added again.
    Release,
}

/// Resolve the conflict of the block `name`, bound to what the user reviewed: the edited
/// block's inner text (`held_sha`, [`inner_sha`]) and the text rematch would write
/// (`desired_sha`; `None` = remove). Either having changed refuses.
pub fn resolve(ledger: &Ledger, paths: &RematchPaths, name: &str, how: Resolve, held_sha: &str, desired_sha: Option<&str>) -> Result<Sync> {
    let ex = exposure(ledger, paths)?;
    if ex.conflicts.iter().any(|c| c == "*") {
        bail!("{}: the file cannot be read or its rematch marker lines do not parse; fix it by hand", ex.target.display());
    }
    if !ex.conflicts.iter().any(|c| c == name) {
        bail!("block {name} in {} is not in conflict", ex.target.display());
    }
    let recs = records(ledger, paths)?;
    let mut set = desired(ledger, paths, &recs, true, None)?;
    if set.blocks.get(name).map(|w| w.sha.as_str()) != desired_sha {
        bail!("what rematch would write for {name} changed since it was shown; review it again: `rematch library inspect instructions-conflict:{name}`");
    }
    let edited = BTreeMap::from([(name.to_string(), if how == Resolve::Restore { Edited::Overwrite } else { Edited::Release })]);
    let released = if how == Resolve::Release { set.blocks.remove(name) } else { None };
    let out = apply(paths, &set, &edited, Some(Expect { name, held_sha }), false)?;
    // Recorded once the reviewed block is released: a refused or failed write decides nothing.
    if let Some(w) = released {
        record(ledger, new_record(paths, Decision::Declined, w.entry.clone(), name, None, &w.sha))?;
    }
    Ok(out)
}

/// `profile/instructions/<name>.md` ← the text approved on this machine for every name a loadable
/// entry holds (within the budget), with its approval as `<name>.decision`. A profile text
/// declined or withdrawn here after the profile's last approval of it goes profile-wide: the `.md`
/// leaves and `<name>.decision` records the withdrawal, so every machine (the one that approved it
/// included) drops those bytes until someone approves them again. A text approved here for entries
/// no longer loadable just leaves. Returns the changed labels. No-op without a profile, and under
/// an omp profile `skip_profiles` excludes: its approvals stay in its own agent dir.
pub fn collect_profile(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<String>> {
    if !crate::profile::present(paths) || skipped_profile(paths) {
        return Ok(Vec::new());
    }
    let dir = profile_dir(paths);
    std::fs::create_dir_all(&dir)?;
    let recs = records(ledger, paths)?;
    let d = desired(ledger, paths, &recs, false, None)?;
    let pd = profile_decisions(paths);
    let publish = |name: &str, sha: &str, decision: Decision, at: UnixMs| -> Result<String> {
        std::fs::write(decision_file(paths, name), serde_json::to_string(&ProfileDecision { sha: sha.to_string(), decision, at })?)?;
        Ok(format!("instructions/{name}.{DECISION_EXT}"))
    };
    let mut changed = Vec::new();
    for w in d.blocks.values().filter(|w| w.source == Source::Ledger) {
        let f = dir.join(format!("{}.md", w.name));
        if std::fs::read(&f).ok().as_deref() != Some(w.text.as_bytes()) {
            std::fs::write(&f, w.text.as_bytes())?;
            changed.push(format!("instructions/{}.md", w.name));
        }
        // Admitted, so approved here after any withdrawal the profile holds: the approval wins.
        if let Some(r) = latest(&recs, &w.name, &w.sha).filter(|r| r.decision == Decision::Approved) {
            if pd.get(&w.name).is_none_or(|p| p.sha != w.sha || p.at < r.at) {
                changed.push(publish(&w.name, &w.sha, Decision::Approved, r.at)?);
            }
        }
    }
    let entries = ledger.list_entries(&EntryFilter::default())?;
    let live = live_entries(paths, &entries);
    for (name, text) in profile_texts(paths) {
        if d.blocks.get(&name).is_some_and(|w| w.source == Source::Ledger) {
            continue;
        }
        let sha = sha256_hex(text.as_bytes());
        let md = dir.join(format!("{name}.md"));
        let declined = latest(&recs, &name, &sha).filter(|r| r.decision == Decision::Declined && !approved_after(&pd, &name, &sha, r.at));
        if let Some(r) = declined {
            if !withdrawn_since(&pd, &name, &sha, r.at) {
                publish(&name, &sha, Decision::Declined, r.at)?;
            }
            std::fs::remove_file(&md)?;
            changed.push(format!("instructions/{name}.md (withdrawn profile-wide)"));
        } else if withdrawn_since(&pd, &name, &sha, 0) {
            std::fs::remove_file(&md)?;
            changed.push(format!("instructions/{name}.md (removed)"));
        } else if suppressed(&recs, &pd, &live, &name, &sha) {
            std::fs::remove_file(&md)?;
            if pd.get(&name).is_some_and(|p| p.sha == sha) {
                std::fs::remove_file(decision_file(paths, &name))?;
            }
            changed.push(format!("instructions/{name}.md (removed)"));
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(name: &str, text: &str) -> Desired {
        Desired { name: name.into(), sha: sha256_hex(text.as_bytes()), text: text.into(), entry: None, source: Source::Ledger, approver: None }
    }

    fn run(cur: &str, want: &[Desired]) -> (String, Sync) {
        let m: BTreeMap<String, Desired> = want.iter().map(|w| (w.name.clone(), w.clone())).collect();
        let mut out = Sync::default();
        let t = plan(cur, &m, &BTreeMap::new(), &mut out).unwrap();
        (t, out)
    }

    #[test]
    fn add_update_remove_restore_the_exact_bytes() {
        for original in ["", "# Mine\n", "# Mine\n\n", "# Mine", "# Mine\r\nline\r\n"] {
            let (one, o) = run(original, &[d("a", "Run the tests.\n")]);
            assert_eq!(o.applied, vec!["a"]);
            assert!(one.starts_with(original), "{one:?}");
            assert!(one.contains("## rematch: a"));
            let (same, o) = run(&one, &[d("a", "Run the tests.\n")]);
            assert_eq!((same.as_str(), o.applied.len() + o.updated.len()), (one.as_str(), 0), "idempotent");
            let (two, o) = run(&one, &[d("a", "Run all tests.\n"), d("b", "B text\n")]);
            assert_eq!((o.updated.clone(), o.applied.clone()), (vec!["a".to_string()], vec!["b".to_string()]));
            assert!(two.contains("Run all tests.") && !two.contains("Run the tests."));
            let (back, o) = run(&two, &[]);
            assert_eq!(o.removed, vec!["a", "b"]);
            assert_eq!(back, original, "removal restores the bytes found");
        }
        let (crlf, _) = run("x\r\n", &[d("a", "t\n")]);
        assert!(!crlf.replace("\r\n", "").contains('\n'), "CRLF file keeps CRLF: {crlf:?}");
    }

    #[test]
    fn edited_blocks_and_bad_markers_are_left_alone() {
        let (one, _) = run("mine\n", &[d("a", "Run the tests.\n")]);
        let edited = one.replace("Run the tests.", "Run the tests twice.");
        let (t, o) = run(&edited, &[]);
        assert_eq!(t, edited);
        assert_eq!(o.conflicts.len(), 1);
        assert_eq!(o.conflicts[0].name, "a");
        assert_eq!(o.conflicts[0].held_sha.as_deref(), Some(inner_sha(&block_text(&edited, "a")).as_str()));
        let (t, o) = run(&edited, &[d("a", "New text\n")]);
        assert_eq!((t, o.conflicts[0].desired_sha.is_some()), (edited.clone(), true));
        let mut out = Sync::default();
        let rel = plan(&edited, &BTreeMap::new(), &BTreeMap::from([("a".to_string(), Edited::Release)]), &mut out).unwrap();
        assert!(!rel.contains("rematch:begin") && rel.contains("Run the tests twice.") && rel.starts_with("mine\n"), "{rel}");
        let broken = one.replace("<!-- rematch:end name=a -->", "");
        let mut out = Sync::default();
        assert!(plan(&broken, &BTreeMap::new(), &BTreeMap::new(), &mut out).is_err());
        let mut out = Sync::default();
        let around = format!("before\n{one}after\n");
        let t = plan(&around, &BTreeMap::new(), &BTreeMap::new(), &mut out).unwrap();
        assert_eq!(t, "before\nmine\nafter\n", "only the block (and its own padding) goes");
    }

    fn block_text(text: &str, name: &str) -> String {
        let p = parse(text).unwrap();
        p.blocks.iter().find(|b| b.name == name).unwrap().inner(text).to_string()
    }

    #[test]
    fn documented_markers_in_fences_and_malformed_ones_are_user_text() {
        let (real, _) = run("", &[d("a", "Run the tests.\n")]);
        let doc = format!("# Notes\n\n```md\n{real}<!-- rematch:end name=zz -->\n```\n\n<!-- rematch:begin not a marker -->\n");
        let (one, o) = run(&doc, &[d("a", "Run the tests.\n")]);
        assert_eq!(o.applied, vec!["a"], "the fenced copy is not a block");
        assert!(one.starts_with(&doc));
        let (back, _) = run(&one, &[]);
        assert_eq!(back, doc);
        let mut out = Sync::default();
        assert!(plan("```\nopen fence\n", &BTreeMap::from([("a".to_string(), d("a", "t\n"))]), &BTreeMap::new(), &mut out).is_err(), "never appends inside a fence");
        assert!(check_text("text with <!-- rematch:begin inline").is_err());
        assert!(check_text("`<!-- rematch:` mid-line").is_err());
    }

    struct Fx {
        t: tempfile::TempDir,
        paths: RematchPaths,
        ledger: Ledger,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let paths = RematchPaths::with_roots(t.path().join("rematch"), t.path().join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        Fx { t, paths, ledger }
    }

    /// A provisional entry `name` in `branch` whose `AGENTS.fragment.md` is `fragment`.
    fn staged(x: &Fx, id: &str, name: &str, branch: &str, fragment: &str) -> Entry {
        let e = crate::ledger::tests::entry(id, name, branch, Status::Provisional);
        let skill = format!("---\nname: {name}\ndescription: Use when testing.\n---\n# {name}\n\nUse when testing.\n");
        let files = [
            crate::types::Edit::Write { path: "SKILL.md".into(), content: skill },
            crate::types::Edit::Write { path: AGENTS_FRAGMENT.into(), content: fragment.into() },
        ];
        library::stage(&x.ledger, &x.paths, &e, &files).unwrap()
    }

    fn sha_of(e: &Entry) -> String {
        fragment(e).unwrap().0
    }

    fn file(paths: &RematchPaths) -> String {
        std::fs::read_to_string(target(paths)).unwrap_or_default()
    }

    fn block_bytes(name: &str, text: &str) -> usize {
        Desired { name: name.into(), sha: sha256_hex(text.as_bytes()), text: text.into(), entry: None, source: Source::Ledger, approver: None }.bytes()
    }

    fn with_profile(paths: &RematchPaths) {
        std::fs::create_dir_all(crate::profile::dir(paths).join(".git")).unwrap();
        std::fs::create_dir_all(profile_dir(paths)).unwrap();
    }

    #[test]
    fn a_same_name_entry_never_keeps_a_retired_entrys_text() {
        let x = fx();
        let a = staged(&x, "ea", "foo", "branch-a", "Text A.\n");
        let b = staged(&x, "eb", "foo", "branch-b", "Text B.\n");
        approve(&x.ledger, &x.paths, &b.id, &sha_of(&b)).unwrap();
        assert!(file(&x.paths).contains("Text B."));
        with_profile(&x.paths);
        assert_eq!(collect_profile(&x.ledger, &x.paths).unwrap(), vec!["instructions/foo.md", "instructions/foo.decision"], "the profile echoes it");

        // B retires while A (same name, other bytes, never approved) stays loadable: neither the
        // ledger record nor the profile echo of B's text stays alive through A's name.
        library::set_status(&x.ledger, &x.paths, &b.id, Status::Retired, Some("test")).unwrap();
        sync(&x.ledger, &x.paths).unwrap();
        assert!(!file(&x.paths).contains("Text B."), "{}", file(&x.paths));
        let recs = records(&x.ledger, &x.paths).unwrap();
        assert!(desired(&x.ledger, &x.paths, &recs, true, None).unwrap().blocks.is_empty());
        assert!(pending(&x.ledger, &x.paths).unwrap().iter().any(|p| p.entry.id == a.id), "A still asks for its own approval");
        assert_eq!(collect_profile(&x.ledger, &x.paths).unwrap(), vec!["instructions/foo.md (removed)"]);
    }

    #[test]
    fn approvals_and_profile_texts_stay_within_the_budget() {
        let x = fx();
        let (small_text, big_text) = ("Keep it short.\n".to_string(), format!("{}\n", "Long rule. ".repeat(80)));
        let small = staged(&x, "e1", "small", "b", &small_text);
        let big = staged(&x, "e2", "big", "b", &big_text);
        let limit = block_bytes("small", &small_text) + block_bytes("big", &big_text) - 1;
        std::fs::create_dir_all(&x.paths.improver).unwrap();
        std::fs::write(x.paths.improver.join("lint.toml"), format!("[limits]\nalways_on_bytes = {limit}\n")).unwrap();
        assert_eq!(budget(&x.paths), limit);

        // Each fits alone (staging checked them one by one); together they do not.
        let p = pending(&x.ledger, &x.paths).unwrap();
        assert!(p.iter().all(|p| p.blocked.is_none()), "{:?}", p.iter().map(|p| &p.blocked).collect::<Vec<_>>());
        approve(&x.ledger, &x.paths, &small.id, &sha_of(&small)).unwrap();
        let p = pending(&x.ledger, &x.paths).unwrap();
        let why = p.iter().find(|p| p.entry.id == big.id).and_then(|p| p.blocked.clone()).expect("the big text is blocked now");
        assert!(why.contains("over the personal-instructions budget"), "{why}");
        let err = approve(&x.ledger, &x.paths, &big.id, &sha_of(&big)).unwrap_err();
        assert!(format!("{err:#}").contains("budget"), "{err:#}");
        let applied = file(&x.paths);
        assert!(applied.contains("Keep it short.") && !applied.contains("Long rule."), "{applied}");

        // A text the profile brings that does not fit waits; the applied block keeps its place.
        with_profile(&x.paths);
        std::fs::write(profile_dir(&x.paths).join("huge.md"), &big_text).unwrap();
        let s = sync(&x.ledger, &x.paths).unwrap();
        assert_eq!(s.over_budget, vec!["huge"]);
        assert!(s.lines().iter().any(|l| l.contains("over the always-on budget")), "{:?}", s.lines());
        assert_eq!(file(&x.paths), applied);

        // A lowered limit never takes an applied block out silently.
        std::fs::write(x.paths.improver.join("lint.toml"), "[limits]\nalways_on_bytes = 10\n").unwrap();
        sync(&x.ledger, &x.paths).unwrap();
        assert_eq!(file(&x.paths), applied);
    }

    #[test]
    fn each_agent_dir_keeps_its_own_approvals_and_state() {
        let x = fx();
        let b = RematchPaths::with_roots(x.paths.root.clone(), x.t.path().join("agent-b"));
        std::fs::create_dir_all(&b.agent_dir).unwrap();
        std::fs::write(target(&b), "").unwrap();
        // The root-wide state of earlier builds said "created": it is not B's (nor A's) to trust.
        std::fs::write(x.paths.root.join(LEGACY_STATE_FILE), r#"{"created":true}"#).unwrap();
        let e = staged(&x, "e1", "rule", "b", "Run the tests.\n");
        let sha = sha_of(&e);

        approve(&x.ledger, &x.paths, &e.id, &sha).unwrap();
        assert!(file(&x.paths).contains("Run the tests."));
        sync(&x.ledger, &b).unwrap();
        assert_eq!(file(&b), "", "an approval for A never writes B");
        assert!(pending(&x.ledger, &b).unwrap().iter().any(|p| p.entry.id == e.id), "B asks on its own");
        assert!(pending(&x.ledger, &x.paths).unwrap().is_empty());

        approve(&x.ledger, &b, &e.id, &sha).unwrap();
        assert!(file(&b).contains("Run the tests."));
        withdraw(&x.ledger, &b, "rule", &sha).unwrap();
        assert!(target(&b).is_file() && file(&b).is_empty(), "B's own empty file stays");
        assert!(file(&x.paths).contains("Run the tests."), "a withdrawal in B leaves A");
        withdraw(&x.ledger, &x.paths, "rule", &sha).unwrap();
        assert!(!target(&x.paths).exists(), "A's file was rematch's: it goes once empty");
        assert!(!x.paths.root.join(LEGACY_STATE_FILE).exists());

        // A failed write is B's alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            approve(&x.ledger, &x.paths, &e.id, &sha).unwrap();
            std::fs::set_permissions(&b.agent_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            let writable = std::fs::write(b.agent_dir.join(".probe"), "").is_ok();
            if !writable {
                approve(&x.ledger, &b, &e.id, &sha).unwrap_err();
                assert!(last_error(&b).is_some() && last_error(&x.paths).is_none());
                assert!(plan_sync(&x.ledger, &b).unwrap().changed, "the drift stays visible");
            }
            std::fs::set_permissions(&b.agent_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            if !writable {
                sync(&x.ledger, &b).unwrap();
                assert!(last_error(&b).is_none() && file(&b).contains("Run the tests."));
            }
        }
    }

    #[test]
    fn concurrent_writers_serialize_and_leave_no_temporaries() {
        let x = fx();
        for i in 0..4 {
            let e = staged(&x, &format!("e{i}"), &format!("rule-{i}"), "b", &format!("Rule {i}.\n"));
            approve(&x.ledger, &x.paths, &e.id, &sha_of(&e)).unwrap();
        }
        std::thread::scope(|s| {
            for _ in 0..6 {
                s.spawn(|| {
                    let l = Ledger::open(&x.paths.ledger_file).unwrap();
                    for _ in 0..4 {
                        uninstall(&x.paths, false).unwrap();
                        sync(&l, &x.paths).unwrap();
                    }
                });
            }
        });
        let text = file(&x.paths);
        for i in 0..4 {
            assert_eq!(text.matches(&format!("<!-- rematch:begin name=rule-{i} ")).count(), 1, "{text}");
        }
        let temps: Vec<_> = std::fs::read_dir(&x.paths.agent_dir).unwrap().flatten().filter(|d| d.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(temps.is_empty(), "{temps:?}");
        uninstall(&x.paths, false).unwrap();
        assert!(!target(&x.paths).exists(), "the created flag survived the races");
    }

    #[test]
    fn a_withdrawal_goes_profile_wide_until_approved_again() {
        let x = fx();
        let tick = || std::thread::sleep(std::time::Duration::from_millis(3));
        // Two omp profiles sharing the root share its profile too (as two synced machines do).
        let b = RematchPaths::with_roots(x.paths.root.clone(), x.t.path().join("agent-b"));
        with_profile(&x.paths);
        let e = staged(&x, "e1", "rule", "br", "Run the tests.\n");
        let sha = sha_of(&e);
        approve(&x.ledger, &x.paths, &e.id, &sha).unwrap();
        assert_eq!(collect_profile(&x.ledger, &x.paths).unwrap(), vec!["instructions/rule.md", "instructions/rule.decision"]);
        sync(&x.ledger, &b).unwrap();
        assert!(file(&b).contains("Run the tests."), "B applies the profile's text");
        let recs = records(&x.ledger, &b).unwrap();
        assert_eq!(desired(&x.ledger, &b, &recs, true, None).unwrap().blocks["rule"].source, Source::Profile);

        // Withdrawn in B: the profile drops the text and A — which approved it — follows.
        tick();
        withdraw(&x.ledger, &b, "rule", &sha).unwrap();
        assert_eq!(collect_profile(&x.ledger, &b).unwrap(), vec!["instructions/rule.md (withdrawn profile-wide)"]);
        sync(&x.ledger, &x.paths).unwrap();
        assert!(!file(&x.paths).contains("Run the tests."), "{}", file(&x.paths));
        assert!(collect_profile(&x.ledger, &x.paths).unwrap().is_empty(), "A does not put it back");
        let p = pending(&x.ledger, &x.paths).unwrap();
        assert!(p.iter().any(|p| p.entry.id == e.id), "A may approve it again");

        // A later approval brings the bytes back everywhere.
        tick();
        approve(&x.ledger, &x.paths, &e.id, &sha).unwrap();
        assert!(file(&x.paths).contains("Run the tests."));
        let c = collect_profile(&x.ledger, &x.paths).unwrap();
        assert_eq!(c, vec!["instructions/rule.md", "instructions/rule.decision"]);
        sync(&x.ledger, &b).unwrap();
        assert!(file(&b).contains("Run the tests."), "the later approval outranks B's earlier withdrawal");
        assert!(collect_profile(&x.ledger, &b).unwrap().is_empty(), "and B does not withdraw it again");
        assert!(collect_profile(&x.ledger, &x.paths).unwrap().is_empty(), "settled");
    }
}
