//! Authoritative schema for rematch. The extension forwards raw events and parses
//! nothing; every durable shape lives here (docs/DESIGN.md §4).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type EntryId = String;
pub type Sha256Hex = String;
pub type UnixMs = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Skill,
    Rule,
    Script,
    Agent,
    Hook,
    Fact,
    Lesson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Draft,
    Provisional,
    Kept,
    Stale,
    Dormant,
    Retired,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Personal,
    Team,
    Fork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    Session { session_id: String },
    Task { task_id: String },
    Import { source: String },
    Description,
}

/// Model family token: `<provider>/<model stem>` where the stem drops version
/// suffixes (`claude-opus-5-5` → `claude-opus`, `gemini-4-argon` → `gemini-argon`,
/// `gpt-6.1-sol-1m` → `gpt-sol`). See `family::family_of`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Family(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// Deterministic trigger predicate (DESIGN §6).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Predicate {
    pub repo_glob: String,
    #[serde(default)]
    pub file_exts: Vec<String>,
    /// ≤ 10 keyword stems.
    #[serde(default)]
    pub stems: Vec<String>,
}

/// The unit of evidence everywhere: one trace record in one session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Moment {
    pub session_id: String,
    pub seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorScope {
    Domain,
    Invariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonTag {
    Invariant,
    Domain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectClass {
    Correction,
    Preference,
    Scope,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub evidence: Moment,
    pub predicted_fix: String,
    pub predicted_regression: String,
    #[serde(default)]
    pub rationale: String,
    pub scope: EditorScope,
}

/// What a compiled hook does in the extension (DESIGN §10). `block` is the original
/// `tool_call` verifier; the other modes are the context policies of D22/D23, which
/// enter as entries under the same counters instead of always-on behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// `tool_call`: block when `input[field]` matches `regex`.
    #[default]
    Block,
    /// `context`: tool results older than `max_age_turns` user turns keep `keep_chars`.
    Truncate,
    /// `tool_call`: the `repeats`-th consecutive identical call (same tool + args hash) is blocked.
    Stuck,
    /// `context`: append the loaded packages' units that overlap the current request.
    Retrieve,
}

/// Declarative rule compiled to `~/.omp/agent/extensions/rematch-hooks/<id>.ts` on
/// approval. Mode parameters are flat optional fields; [`HookRule::params`] resolves
/// them with the mode defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRule {
    #[serde(default)]
    pub mode: HookMode,
    /// Tool the rule applies to (`*` = every tool; ignored by `retrieve`).
    pub tool: String,
    /// Input field inspected by `block` (e.g. `command`, `path`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub field: String,
    /// `block` only: Rust regex (a leading `(?i)` becomes the JS `i` flag).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub regex: String,
    /// Block reason (`block`, `stuck`); optional description otherwise.
    #[serde(default)]
    pub message: String,
    pub pass_condition: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_chars: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeats: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_overlap: Option<u32>,
}

/// Resolved mode parameters of a [`HookRule`] (the `params` of the compiled module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum HookParams {
    Block {},
    Truncate { max_age_turns: u32, keep_chars: u32 },
    Stuck { repeats: u32 },
    Retrieve { budget_bytes: u32, min_overlap: u32 },
}

pub const DEFAULT_TRUNCATE_MAX_AGE_TURNS: u32 = 6;
pub const DEFAULT_TRUNCATE_KEEP_CHARS: u32 = 1200;
pub const DEFAULT_STUCK_REPEATS: u32 = 3;
pub const DEFAULT_RETRIEVE_BUDGET_BYTES: u32 = 6000;
pub const DEFAULT_RETRIEVE_MIN_OVERLAP: u32 = 2;

impl HookRule {
    /// `block` rule (the shape the editor proposes).
    pub fn block(tool: &str, field: &str, regex: &str, message: &str, pass_condition: &str) -> HookRule {
        HookRule {
            mode: HookMode::Block,
            tool: tool.into(),
            field: field.into(),
            regex: regex.into(),
            message: message.into(),
            pass_condition: pass_condition.into(),
            max_age_turns: None,
            keep_chars: None,
            repeats: None,
            budget_bytes: None,
            min_overlap: None,
        }
    }

    pub fn params(&self) -> HookParams {
        match self.mode {
            HookMode::Block => HookParams::Block {},
            HookMode::Truncate => HookParams::Truncate {
                max_age_turns: self.max_age_turns.unwrap_or(DEFAULT_TRUNCATE_MAX_AGE_TURNS),
                keep_chars: self.keep_chars.unwrap_or(DEFAULT_TRUNCATE_KEEP_CHARS),
            },
            HookMode::Stuck => HookParams::Stuck { repeats: self.repeats.unwrap_or(DEFAULT_STUCK_REPEATS) },
            HookMode::Retrieve => HookParams::Retrieve {
                budget_bytes: self.budget_bytes.unwrap_or(DEFAULT_RETRIEVE_BUDGET_BYTES),
                min_overlap: self.min_overlap.unwrap_or(DEFAULT_RETRIEVE_MIN_OVERLAP),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub kind: Kind,
    pub name: String,
    pub branch: String,
    pub scope: Scope,
    pub status: Status,
    pub family: Family,
    #[serde(default)]
    pub verified_on: Vec<Family>,
    #[serde(default)]
    pub helpful: u32,
    #[serde(default)]
    pub harmful: u32,
    #[serde(default)]
    pub evidence: Vec<Moment>,
    pub origin: Origin,
    pub predicate: Predicate,
    pub trigger_line: String,
    #[serde(default)]
    pub redirect_class: Option<RedirectClass>,
    #[serde(default)]
    pub predecessor: Option<EntryId>,
    #[serde(default = "one")]
    pub version: u32,
    /// Directory holding SKILL.md / rematch.json (absolute).
    pub path: PathBuf,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
    #[serde(default)]
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub lesson_tag: Option<LessonTag>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hook: Option<HookRule>,
    /// Scripts approved for execution (relative paths inside the entry dir).
    #[serde(default)]
    pub approved_scripts: Vec<String>,
    /// Set when this entry was created under an existing one in a sibling slot.
    #[serde(default)]
    pub sibling_of: Option<EntryId>,
    /// When `status` last changed (unix ms); `None` ⇒ `created_at`. Drives the
    /// 30-day dormant and kept→retired windows (`counters::decide`).
    #[serde(default)]
    pub status_since: Option<UnixMs>,
    /// Why the last status change happened (rollback note, disable reason, decline).
    #[serde(default)]
    pub status_note: Option<String>,
    /// Batch winner staged provisional by a `run` of its `Origin::Task`: the next
    /// worker run of that task keeps it on a verifier pass, retires it on a fail.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_next_run: bool,
    /// Fork entries: repo-relative path the entry's file is shipped to in the fork
    /// (`ship --scope fork`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_path: Option<String>,
    /// Fork repo the entry was last shipped to (`ship --scope fork`); drift check target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_repo: Option<PathBuf>,
    /// `git rev-parse HEAD` of `fork_repo` when shipped; a different HEAD ⇒ `stale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_commit: Option<String>,
    /// Families this entry no longer serves after a per-family split (Phase 3). Entries
    /// load regardless of family (SPEC §Models), so this is informational: `match`
    /// reports it and the harmed family gets its own sibling entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub families_excluded: Vec<Family>,
    /// Provenance links (`transfers_from` for cross-branch transfers and exports).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyCounters {
    pub matched: u32,
    pub recurrence: u32,
    pub harm: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub matched: u32,
    pub recurrence: u32,
    pub harm: u32,
    pub tokens: u64,
    pub turns: u32,
    #[serde(default)]
    pub per_family: BTreeMap<Family, FamilyCounters>,
    /// Last 30 days of harm events (unix ms), for the kept→retired rule.
    #[serde(default)]
    pub harm_times: Vec<UnixMs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Keep,
    Retire,
    Dormant,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    Write { path: String, content: String },
    Delete { path: String },
}

/// Editor output. Deliberately has no score field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub edits: Vec<Edit>,
    pub manifest: Manifest,
    #[serde(default)]
    pub cites: Vec<Sha256Hex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintReport {
    pub ok: bool,
    pub findings: Vec<LintFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub matched: u32,
    pub recurrence: u32,
    pub harm: u32,
    pub family: Family,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass { score: Option<f64>, detail: String },
    Fail { score: Option<f64>, detail: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Certificate {
    pub lint: LintReport,
    #[serde(default)]
    pub canary: Option<Paired>,
    #[serde(default)]
    pub verifier: Option<Verdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    Personal,
    Prompted,
    Pr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub certificate: Sha256Hex,
    pub entry: EntryId,
    pub activation: Activation,
}

// ---- Session outcome records (SPEC §"Session outcome records") ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Change,
    Hypothesis,
    Decision,
    Artifact,
    Lesson,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRef {
    #[serde(rename = "type")]
    pub ty: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// Dotted path, prefix-seekable: `quant.options`, `repo.payments`.
    pub domain: String,
    #[serde(default)]
    pub entities: Vec<EntityRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facet {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Test,
    Receipt,
    Review,
    Redirect,
    Metric,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub verdict: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Kept,
    Killed,
    Inconclusive,
    Shipped,
    Reverted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeResult {
    pub status: ResultStatus,
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    #[serde(default)]
    pub kill_criterion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkRel {
    Supersedes,
    DependsOn,
    Contradicts,
    TransfersFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub rel: LinkRel,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default)]
    pub self_reported: Vec<String>,
    #[serde(default)]
    pub data_snapshot: Option<String>,
    #[serde(default)]
    pub tool_versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub session_id: String,
    pub ts: UnixMs,
    pub machine: String,
    pub model_family: Family,
    pub repo: String,
    pub branch: String,
    pub kind: OutcomeKind,
    pub subject: Subject,
    pub claim: String,
    /// hash(sorted normalized params): the exact "same thing" key.
    pub signature: Sha256Hex,
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub facets: Vec<Facet>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    pub result: OutcomeResult,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub provenance: Provenance,
}

// ---- Ledger session row (what the importer and the trace parser both produce) ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub cwd: String,
    #[serde(default)]
    pub repo: Option<String>,
    pub branch: String,
    pub started_at: UnixMs,
    #[serde(default)]
    pub ended_at: Option<UnixMs>,
    #[serde(default)]
    pub family: Option<Family>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tool_calls: u32,
    pub tool_errors: u32,
    /// Skill names read (via `skill://` or `skills/<name>/SKILL.md`) or invoked (`/skill:`).
    #[serde(default)]
    pub skill_reads: Vec<String>,
    #[serde(default)]
    pub files_touched: Vec<String>,
    #[serde(default)]
    pub exts: Vec<String>,
    #[serde(default)]
    pub stems: Vec<String>,
    #[serde(default)]
    pub redirects: Vec<Redirect>,
    #[serde(default)]
    pub error_clusters: u32,
    /// Seq of the tool result that tipped each error cluster into existence (≥3 errors in a
    /// window of 6 consecutive tool results). `error_clusters == error_cluster_seqs.len()`.
    #[serde(default)]
    pub error_cluster_seqs: Vec<u32>,
    #[serde(default)]
    pub takeovers: u32,
    #[serde(default)]
    pub loaded_entries: Vec<EntryId>,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub counted: bool,
    /// `"trace"` (extension) or `"import"` (omp JSONL).
    pub source: String,
    /// omp session file an imported row was read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// When `reflect --history` dealt with this imported session (handed it to reflection or
    /// ruled it out). Survives re-imports and re-folds (`Ledger::upsert_session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_reflected_at: Option<UnixMs>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub session_id: String,
    pub seq: u32,
    pub prelabel: String,
    #[serde(default)]
    pub class: Option<RedirectClass>,
    #[serde(default)]
    pub confidence: Option<f64>,
    pub text_head: String,
    #[serde(default)]
    pub deliverable_head: String,
    pub t: UnixMs,
}
