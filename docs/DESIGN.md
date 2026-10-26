# rematch — implementation contract

Source of truth for every slice. The build spec is `docs/SPEC.md`; this file records
the verified omp facts, the deviations forced by them, and the exact interfaces the
crates, the extension and the improver share. When SPEC and DESIGN disagree, DESIGN
wins (it exists only where a technical constraint was verified).

## 0. Verified omp facts (omp v18.12.6; `omp://extensions.md`, `omp://skills.md`, probes)

| Fact | Consequence |
| --- | --- |
| `resources_discover` is implemented in `ExtensionRunner` but **no `AgentSession` callsite emits it** | Cannot contribute `skillPaths` at runtime. Replaced (see §1). |
| `lookup("skills.customDirectories").override(pi.pi.settings, [...])` executed **inside the extension factory** lands before `loadSkills()`: probe showed the custom-dir skill in `before_agent_start.systemPrompt` | Load mechanism: factory asks the binary which staging dirs match `cwd`, then overrides the setting additively (`[...configured, ...ours]`). Decision holds for the process (spec already caveats). |
| In the TUI a plugin extension that registers no provider/shortcut/composer-shape/file-fallback may be lazy-loaded after the composer accepts input | Extension registers one shortcut (digest) so it loads eagerly, before discovery. |
| `session_shutdown` handlers get 2 s, run concurrently | Shutdown only writes a flag file; nothing long-running. |
| `pi.exec(cmd, args, {cwd, timeout, signal})` → `{stdout, stderr, code, killed}`; buffered; available during factory load | Short binary calls only. Long work: binary daemonizes itself (`setsid`) and returns a job id. |
| `tool_result` event: `{toolName, toolCallId, input, content, details, isError}`; no "active skills" field | Skill activity inferred from `read` of `skill://<name>` / `.../skills/<name>/SKILL.md` and `/skill:` tokens in input. |
| `input` event: `{text, images, source}` at ingress, interactive+rpc only | Trace user turns here. `ctx.isIdle()` false at input ⇒ takeover/steer. |
| Session JSONL: `~/.omp/agent/sessions/<slug>/<ts>_<uuid>.jsonl`; line 1 is a padded `{"type":"title"}` slot, line 2 `{"type":"session","version":3,"id","timestamp","cwd"}`; entries are a tree (`id`,`parentId`); `message.role` ∈ `user|assistant|toolResult` (camelCase); tool calls are `{"type":"toolCall","id","name","arguments"}` blocks in assistant content; assistant carries `provider`,`model`,`usage`,`stopReason`; `toolResult` carries `toolCallId`,`toolName`,`content`,`details.resolvedPath`,`isError` | Importer must locate the `session` record (not index 0), reconstruct the active branch from the last entry, and tolerate unknown `type`s. |
| `--mode json -p --no-tools --model <provider/id>` emits JSONL events; assistant messages appear under `message_start|message_update|message_end|assistant_message` (`event.message.role=="assistant"`) and `agent_end.messages[]`; each has `provider`, `model`, optional `upstreamProvider/upstreamModel`, `errorMessage`, `stopReason` | EditorChannel verifies provider+model on **every** assistant message (forge `omp_runner` contract). |
| Custom `modelRoles` keys are allowed in `~/.omp/agent/config.yml` and `<repo>/.omp/config.yml` | `rematch-editor` / `rematch-classifier` roles read by the binary from config.yml (no `modelRoles` API for externals). Fallbacks: editor→`slow`→`default`; classifier→`smol`→`tiny`→`default`. |
| Native extension roots: `~/.omp/agent/extensions/{*.ts, <dir>/index.ts}`; a subdir's non-index `*.ts` is **not** discovered | `~/.omp/agent/extensions/rematch-hooks/<id>.ts` files are inert to omp; the main extension `tool_call` handler dynamically imports them (the spec's "run approved compiled verifiers"). |
| `omp plugin install <local-path>` links the dir into `~/.omp/plugins/node_modules/<pkg.name>` and enables it in `omp-plugins.lock.json`; `package.json#omp.extensions` lists entries | Install: `omp plugin link <root>/extension` (or `install`). Binary on PATH via `~/.local/bin/rematch` shim written by `scripts/install.sh`. |
| `omp anonymize` writes a shareable redacted copy of a session | `tests/goldens/` holds anonymized real sessions for the importer tests. |
| Credentials, `agent.db`, `models.db` live in the agent dir | Batch isolation does **not** swap `PI_CODING_AGENT_DIR` (would lose auth). Isolation = fresh git worktree + `--config <overlay.yml>` (all skill providers off, `skills.customDirectories=[candidate]`, memory off) + `--no-extensions --no-rules --no-session`. |

## 1. Topology

```
extension/index.ts  ──pi.exec JSON envelope──▶  rematch (Rust, static)
   │ writes ~/.rematch/traces/<session>.jsonl           │ owns ledger.sqlite, staging/, library, improver, jobs
   │ writes ~/.rematch/pending/<session>.json            │ spawns `omp -p --no-tools --mode json` (editor, classifier)
   │ factory: `match --cwd` → skills.customDirectories   │ writes staging/<branch>/<name>/ and ~/.omp/agent/skills/<name>/
   └ tool_call: runs ~/.omp/agent/extensions/rematch-hooks/<id>.ts
```

Root `REMATCH_ROOT` = `$REMATCH_ROOT` or `~/.rematch`. Agent dir = `$PI_CODING_AGENT_DIR` or `~/.omp/agent`.
`REMATCH=0` or `REMATCH_ROOT/off` file ⇒ extension registers nothing but `/rematch on` and the `rematch` tool; binary refuses reflect/stage.

### Files under REMATCH_ROOT

```
traces/<session_id>.jsonl         extension trace (schema §3)
pending/<session_id>.json         {session_id, flagged_at, reasons:[...], trace}   written at shutdown; deleted by reflect
ledger.sqlite  blobs/<sha256>     ledger (schema §5) + content-addressed blobs
staging/<branch>/<name>/          provisional entries (loaded via customDirectories)
drafts/<name>/                    draft entries (never loaded)
archive/<status>/<id>/            retired|dormant|stale entries (never loaded, never deleted)
predecessors/<name>/<version>/    copies of replaced kept entries (rollback)
improver/{editor.md,reflector.md,lint.toml,omp-no-tools.yml}   copied from repo on first run; EXPERIENCE.md files live next to entries
jobs/<job_id>.json  jobs/<job_id>.log      detached job state
runs/<task>/<seed>/<variant>/     batch workspaces {wt/, skills/, overlay.yml, events.jsonl} (deleted after receipt)
runs/<task>/receipt.json          last batch receipt;  runs/<task>/traces/<session>.jsonl  reference traces for `create --from task`
tasks/<task_id>.json              batch task definitions
branches/<branch>/domain.yml      facet vocabulary per branch
export/                           git-tracked work↔home export of invariant entries
off                               kill switch marker
```

## 2. CLI (binary `rematch`)

Every subcommand prints exactly one JSON line on stdout:
`{"ok":true,"operation":"<op>","data":{...}}` (exit 0) or `{"ok":false,"operation":"<op>","error":"..."}` (exit 1; 2 usage; 130 cancelled).
Human-readable output only with `--text`. Logs go to stderr.

```
doctor                                     omp binary+version, roles resolved (provider/model), extension linked?, ledger schema version, root paths, off?
import --history [--sessions <dir>] [--forge <path> [--migrate]]
reflect --pending [--session <id>] [--editor <cmd>] [--detach]   idempotent per pending file (lock `pending/.lock`); --editor overrides every model call (tests); --detach → {job_id}
match --cwd <dir> [--inputs <json>]        {branch, repo, skill_dirs:[...], entries:[{id,name,status,predicate_hit}], hooks:[path...], off:bool}
digest --cwd <dir>                         {lines:[...], approvals:[{id,name,kind,diff,reason}], review:[...capability floor...], transfers:[...]}
record --session <id> --event <json>       accepts `{"ev":"mark","class":…}` only (= mark); every other event belongs in the trace file
create <name> --from sessions|task|description [--task <id>] [--branch <b>] [--description <text>] [--files <p>...] [--editor <cmd>]
improve <skill> [--split-only] [--branch <b>] [--editor <cmd>]
activate <name> [--task <id> | --unverified]
run --task <id> --verifier <cmd> [--width k] [--seed n]  → {job_id}
run-worker --job <id>                      internal, detached
status [job]
library ls | why [<session_id>] | disable <id> | rollback <skill> | pin <id> | unpin <id> | approve <id> | decline <id> | export [--to <dir>] | import-export [--from <dir>]
task add <id> --repo <path> --prompt <text> [--verifier <cmd>] [--max-time 10m] [--tools ...]
transfer suggest | transfer apply <lesson_id> --branch <b>
ship --scope team|fork [--repo <path>] [--entry <id>...]
count [--session <id>] [--editor <cmd>]    fold finished traces into counters and decide (also run by reflect --pending)
mark --session <id> --class correction|preference|scope
off | on
uninstall [--yes]
```

`reflect --pending` performs, in order: count (fold all finished traces not yet counted → counters → decide → EXPERIENCE.md), then one reflection per pending file respecting rate limits.

## 3. Trace schema (extension → binary)

`traces/<session_id>.jsonl`, one JSON object per line, written append-only by the extension:
`{"seq": <u32>, "t": <unix ms>, "ev": "<name>", ...}`. The extension never reads it back.

| ev | fields |
| --- | --- |
| `session_start` | `session_id, cwd, repo (git toplevel or null), branch_hint (basename), model {provider,id}, mode, agent {kind,name}, omp_version, loaded_entries [id...], hooks_loaded [id...], staging_dirs [...]` |
| `input` | `text (≤16 KB), source, idle (bool), after_deliverable (last assistant text ≤2 KB or null), prelabel ∈ {"none","interrupt","revert","reprompt","negation","mark:<class>"}, skills_invoked [names from /skill: tokens]` |
| `tool_result` | `tool, call_id, input (JSON ≤8 KB, keys kept, long strings truncated with "…[+N]"), args_hash (sha256 of canonical JSON of full input), result_bytes, is_error, error_class (null | "error"), content_head (first 1 KB of text content), skill_read (name or null), path (input.path/file_path when present)` |
| `tool_blocked` | `tool, call_id, hook_id, reason` |
| `turn_end` | `model {provider,id}, usage {input,output,cacheRead,cacheWrite,totalTokens,cost}, stop_reason, text_head (≤2 KB)` |
| `agent_end` | `turns, is_terminal, yielded` |
| `model_change` | `from {provider,id} , to {provider,id}, reason` (from `retry_fallback_applied` or turn-to-turn change) |
| `mark` | `class ∈ correction|preference|scope` |
| `session_shutdown` | `reasons [...], flagged (bool)` |

Pre-classification (extension, no model; regex constants live in `rematch-core/src/redirect.rs` and are copied verbatim into the extension): `mark` = `/^\/rematch\s+mark\s+(correction|preference|scope)\b/i`; `interrupt` = input while `!ctx.isIdle()`; `revert` = `/\b(?:revert|undo|roll ?back|put it back|git restore)\b|\bgit checkout --(?:\s|$)/i`; `negation` = `/^(?:no|nope|don't|do not|stop|not that|wrong|that's (?:not|wrong)|that is wrong|this is wrong|incorrect|you didn't|you did not|not what i|i said|actually|instead)\b/i` (leading whitespace trimmed); `reprompt` = Jaccard(tokens(text), tokens(previous input)) ≥ 0.6 with tokens `[a-z0-9]+` on lowercased text. Precedence: mark > interrupt > revert > negation > reprompt > none.
Session is flagged when any of: ≥1 prelabel ≠ none; `is_error` tool results ≥ 3 in any window of 6 consecutive tool results (tool-error cluster); a `mark`. Flag = write `pending/<session_id>.json`.

## 4. Core types (`rematch-core/src/types.rs`, authoritative in Rust)

```rust
pub enum Kind { Skill, Rule, Script, Agent, Hook, Fact, Lesson }
pub enum Status { Draft, Provisional, Kept, Stale, Dormant, Retired, Pinned }
pub enum Scope { Personal, Team, Fork }
pub enum Origin { Session{session_id}, Task{task_id}, Import{source}, Description }
pub struct Family(String)                       // "anthropic/claude", "github-copilot/gemini", … (provider + model stem, see family())
pub struct Predicate { repo_glob: String, file_exts: Vec<String>, stems: Vec<String> /* ≤10 */ }
pub struct Moment { session_id: String, seq: u32 }
pub struct Manifest { evidence: Moment, predicted_fix: String, predicted_regression: String, rationale: String, scope: EditorScope }
pub enum EditorScope { Domain, Invariant }
pub struct Entry { id, kind, name, branch, scope, status, family: Family, verified_on: Vec<Family>, helpful: u32, harmful: u32,
                   evidence: Vec<Moment>, origin, predicate, trigger_line: String, redirect_class: Option<RedirectClass>,
                   predecessor: Option<EntryId>, version: u32, path: PathBuf, created_at, updated_at, manifest: Option<Manifest>,
                   lesson_tag: Option<LessonTag /* Invariant|Domain */>, pinned: bool }
pub struct Counters { matched: u32, recurrence: u32, harm: u32, tokens: u64, turns: u32, per_family: BTreeMap<Family, FamilyCounters> }
pub enum Decision { Keep, Retire, Dormant, Continue }
pub struct Proposal { edits: Vec<Edit>, manifest: Manifest, cites: Vec<String /* sha256 */> }   // no score field
pub enum Edit { Write{path, content}, Delete{path} }                                           // paths relative to entry dir
pub struct LintReport { ok: bool, findings: Vec<LintFinding{rule, severity: Error|Warn, message, path}> }
pub struct Certificate { lint: LintReport, canary: Option<Paired>, verifier: Option<Verdict> }
pub struct Receipt { certificate: String /* sha256 */, entry: EntryId, activation: Activation /* Personal|Prompted|Pr */ }
pub enum RedirectClass { Correction, Preference, Scope, None }
pub struct Outcome { … exactly SPEC §"Session outcome records" … }
```

Entry directory layout (`<dir>` = staging/drafts/archive/kept path):
```
<dir>/SKILL.md            frontmatter: name, description, rematch: {id, status, branch, family, version, predicate_hash}
<dir>/rematch.json        serde of Entry + manifest + predicate (the ledger row mirrors it; rematch.json is authoritative for files, ledger for counters)
<dir>/units/*.md  facts/*.md (each fact has `as_of:`)  scripts/* (mode 0644 until approved; 0755 after)
<dir>/hook.json           kind=Hook only: {tool, field, regex, message, pass_condition}
<dir>/EXPERIENCE.md       written by the reflector after keep/retire (read-only to the system)
```

## 5. Ledger (SQLite, `rusqlite` bundled)

Tables (all with `created_at` ms): `schema_version`; `sessions(id PK, cwd, repo, branch, started_at, ended_at, family, provider, model, turns, tokens_in, tokens_out, tool_calls, tool_errors, skill_reads JSON, files_touched JSON, exts JSON, stems JSON, redirects JSON, flagged, counted, source 'trace'|'import')`; `redirects(id, session_id, seq, prelabel, class, confidence, text_head, deliverable_head)`; `entries(id PK, json)` + `entry_status(id, status, branch, name, predicate_hash, kind, family)`; `counters(entry_id, session_id, family, matched, recurrence, harm, tokens, turns, PRIMARY KEY(entry_id, session_id))`; `proposals(id, entry_id, session_id, blob, lint_ok, created_at)`; `receipts(id, entry_id, certificate_hash, activation)`; `admissions(predicate_hash, day, entry_id)`; `jobs(id, kind, status, json)`; `outcomes(id, session_id, ts, machine, family, repo, branch, kind, domain, subject_json, claim, signature, params_json, evidence_json, result_json, links_json, provenance_json, status)` with indexes `exact(signature)`, `composite` via `outcome_facets(outcome_id, domain, key, value)` indexed `(domain,key,value)`, `temporal(subject_id, ts)`; `minhash(outcome_id, band, hash)`.

## 6. Predicate semantics (deterministic, same everywhere)

Derivation (binary, on stage): `repo_glob` = the originating session's repo path (exact, as recorded by the extension from `git rev-parse --show-toplevel`) or `**` if no repo; `file_exts` = extensions of files read/edited in the evidence window (≤8); `stems` = up to 10 Porter-stemmed, stop-worded, lowercase tokens ranked by tf-idf against the ledger's session corpus over a weighted document: the originating task turn (the last user input before the redirect) ×2 + basenames of touched files ×2 + the redirect turn ×1. The redirect's own wording ("that's wrong", "every") must not crowd out the task's domain terms, or later sessions on the same task never count (observed on the first real run; fixed in `reflect.rs`). The editor's `entry.stems` are used only when derivation yields nothing: the pre-editor predicate hash is what the rate limits key on.
Load-time match (`match --cwd`): glob matches repo (or cwd) ⇒ branch dir contributed when ≥1 provisional entry in that branch matches. Both the given path and its `canonicalize()`d form are tried (macOS `/tmp` → `/private/tmp`), on either side.
Triage (`trace::triage`) has two independent axes: *kind* process|content decides whether to propose (content = a corrected prose answer with no tool activity and no process words; anything the agent produced with tools, tool-error clusters, takeovers, reverts and procedural text are process); *scope* domain|invariant picks the editor (domain by default; invariant for error clusters, takeovers, reverts, or text about the agent's general way of working, `INVARIANT_TEXT_PATTERN`). After classification, a session whose only redirects are scope|none and that has no cluster/takeover/mark carries no signal: no editor call.
Count-time match (binary, from a finished trace): glob hit AND (`file_exts` empty OR overlap ≥1) AND (`stems` empty OR overlap ≥ min(2, stems.len())). Inputs = first 5 `input` texts + all `tool_result.path` values.

## 7. Counters and decisions (SPEC §loop 7–8, verbatim)

Per matched session (predicate hit while the entry was in `loaded_entries`): `recurrence += 1` if a redirect of the entry's `redirect_class` occurred (classifier output, or prelabel ∈ {revert, negation, mark} when class unknown); `harm += 1` if a new tool-error cluster, an `interrupt`, or a user `disable` happened. Tokens/turns recorded.
Decide: `matched ≥ 4 && recurrence == 0 && harm == 0 → Keep`; `recurrence ≥ 2 || harm ≥ 2 → Retire`; `matched ≥ 12 → Dormant`; `provisional for 30 days && matched < 3 → Dormant`; `Kept && harm ≥ 2 within 30 days → Retire`; else `Continue`. Per-family breakdown kept; if one family shows harm ≥ 1 and another has matched ≥ 2 with zero harm/recurrence, the entry is split (Phase 3): the harmed family gets a sibling proposal.
Implementation notes (`counters.rs`): each matched session contributes 0/1 recurrence and 0/1 harm (one row per (entry, session)). Prelabel mapping when the classifier gave no class: `revert|negation|mark` → correction, `mark:<class>` → that class; `interrupt|reprompt|none` → no class; a classifier `none` means no class. An entry without a `redirect_class` (motivated by an error cluster or takeover) recurs on any classed redirect. Harm = `error_clusters > 0 || takeovers > 0 || disabled`. Rule order: pinned ⇒ Continue; provisional: Retire, Keep, Dormant(12), Dormant(30 d since `status_since`, < 3 matched); kept: ≥ 2 harm events in the last 30 days and since it became kept ⇒ Retire. Count-time match uses `repo` (else `cwd`) against the glob, where a glob also matches descendants of the path it names.

## 8. Rate limits

Per predicate_hash: ≤1 admission per UTC day; no reflection while a provisional entry with the same predicate_hash exists (pending canary). Per branch: ≤10 provisional; 5 kept general + 5 kept per branch before a new candidate must merge/revise (near-duplicate by simhash(normalized text) Hamming ≤ 6 or identical script body ⇒ append evidence to existing entry). ≤3 model calls per reflection (classifier batch, editor, one lint-retry).
Implementation (`rate::can_admit`, checks in this order): pending canary (any provisional entry with the predicate_hash) → `PendingCanary`; an admission today (UTC) → `DailyPredicate`; ≥ 10 provisional in the branch → `BranchProvisionalCap`; kept cap → `NeedsMerge` (for branch `shared`: ≥ 5 kept in `shared`; otherwise ≥ 5 kept in the branch). `NeedsMerge` is a routing signal: the caller merges/revises via `dedup::route_duplicate` instead of dropping the candidate.

## 9. Editor channel (`omp -p`)

argv (forge contract): `omp --cwd <tmp> --model <provider/id> --no-tools --system-prompt <improver/editor.md> --config <improver/omp-no-tools.yml> --no-prewalk --no-session --no-title --no-extensions --no-skills --no-rules --no-lsp --mode json -p -- <prompt>`; env adds `PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0`; own process group; timeout (default 180 s) ⇒ SIGTERM group, 2 s, SIGKILL group. Every assistant message must match provider+model (and upstream* when present); any mismatch, `errorMessage`, `stopReason ∈ {error, aborted}`, or toolCall block ⇒ failed run (never a fallback).
`--editor <cmd>` (tests) replaces omp: the command receives the prompt on stdin and prints the reply.

Editor prompt input (one JSON document in the user prompt, wrapped as data):
```json
{"scope":"domain|invariant","trace_excerpt":[...trace records around the moment...],"deliverable":"...","redirect":{"class":"...","text":"..."},
 "current_entry":{"files":{"SKILL.md":"..."}}|null,"library":[{"name","trigger_line","experience":"..."}],"lint":{"rules":[...]},"retry_findings":[...]|null}
```
Editor reply: one fenced ```json block:
```json
{"decision":"EDIT|KEEP_BASE","entry":{"kind":"skill|hook|script|fact|lesson","name":"kebab","description":"...","trigger_line":"Use when ...",
 "files":{"SKILL.md":"...","units/x.md":"...","scripts/x.sh":"..."},"hook":{...}|null,"stems":[...]},
 "manifest":{"evidence":{"session_id":"...","seq":0},"predicted_fix":"...","predicted_regression":"...","rationale":"..."}}
```
Classifier prompt: batched `[{id, text, after_deliverable, prelabel}]` → ```json `[{"id":..,"class":"correction|preference|scope|none","confidence":0-1}]```.

## 10. Extension (`extension/index.ts`, no build step)

Factory (async): read env/off; resolve binary (`$REMATCH_BIN` → `<ext>/../target/release/rematch` → `<ext>/../target/debug/rematch` → PATH); `await pi.exec(bin, ["match","--cwd",process.cwd()])`; override `skills.customDirectories` with `[...configured, ...skill_dirs]`; remember `entries`/`hooks`; register tool `rematch {operation, arguments[]}` (approval: read for doctor/status/library/digest/match/why, exec otherwise), command `/rematch`, shortcut `alt+e` (digest), handlers §3. `session_start`: open trace, write `session_start`, spawn `reflect --pending` detached (`Bun.spawn` with `stdio ignore`, `unref`), run `digest` and present lines via `ctx.ui.notify`, and each approval via `ctx.ui.confirm(title, diff)` → `library approve|decline <id>`. `tool_call`: evaluate loaded hook modules (`default export {id, tool, test(input)=>string|null}`); block with the rule's message, record `tool_blocked`. `/rematch <sub>`: `why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|ship|transfer` map 1:1 onto the binary; long ones return a job id.

## 11. Deviations from SPEC (all forced by §0)

1. `resources_discover` → factory-time `skills.customDirectories` override (same semantics, per process).
2. Batch isolation uses a `--config` overlay + fresh worktree instead of a private `PI_CODING_AGENT_DIR` (auth lives in the agent dir).
3. `omp plugin install @<org>/rematch` → `scripts/install.sh` (`cargo build --release`, `omp plugin link extension/`, PATH shim). No registry publish.
4. "Related thing: embeddings, exploratory only" is not implemented: the binary holds no provider credentials; exact signature + MinHash are implemented.
5. Status `pinned` is carried as the `Entry.pinned` flag (status and location unchanged; automatic transitions refuse pinned entries; `disable`/`rollback` are user actions and proceed). `Status::Pinned` remains a valid status for imports.
6. A kept entry replaced by a sibling becomes `dormant` (files in `predecessors/<name>/<version>/`) rather than `retired`, because `rollback` must be able to restore it and `retired` never leaves. A non-rematch skill that a kept entry replaced is saved as version `0` and restored by `rollback`/`uninstall`.
7. Declining an approval (`library decline`) retires the entry (SPEC: retired ← "approval declined").

## 12. lint.toml schema

`improver/lint.toml` (copied to `REMATCH_ROOT/improver/` on first run). Every key is optional; a missing key takes the built-in value (`rematch_core::lint::DEFAULT_LINT_TOML`, which the shipped file equals verbatim — a test enforces it). All `[patterns]` entries are Rust `regex` syntax; use TOML multi-line literal strings (`'''…'''`) for patterns containing `'` (inside `'…'`, `''` is **not** an escape).

```toml
[limits]
always_on_bytes = 8192   # SKILL.md bodies (frontmatter excluded) per branch, this entry + LintContext.branch_other_bytes
max_stems = 10           # predicate.stems
max_exts = 8             # predicate.file_exts

[patterns]               # rule ids in brackets
session_id = [...]       # [identifiers] UUIDs (v4/v7), omp session file names
user_names = [...]       # [identifiers] home-dir paths that reveal a user; plus LintContext.user_names (literal, case-insensitive, word-bounded)
task_paths = [...]       # [identifiers] task-specific absolute paths (/Users, /home, /tmp, /var/folders)
urls = [...]             # [executable_risk]
network = [...]          # [executable_risk] curl, wget, nc, ssh, scp
pipe_shell = [...]       # [executable_risk] curl|wget … | sh
destructive_git = [...]  # [executable_risk] push -f/--force, reset --hard, clean -fd
string_match_user = [...] # [string_match_user] branching on the user's literal words
approval = [...]         # manifest.rationale matching any ⇒ executable_risk findings are Warn ("approval requested"), else Error
order_exception = [...]  # manifest.rationale matching any ⇒ form_order findings are Warn, else Error

[form]
order = ["trigger", "laws", "procedure", "pitfalls", "facts"]
[form.headings]          # lowercase substrings of a ##-heading that classify it; unclassified headings are ignored
trigger = ["trigger", "when to use", "scope"]
laws = ["laws", "rules", "invariants", "constraints"]
procedure = ["procedure", "workflow", "steps"]
pitfalls = ["pitfalls", "anti-patterns", "gotchas"]
facts = ["facts", "workspace facts", "reference"]

[hooks]
probes = ["echo ok", "cargo test", "git status", "README.md", "src/lib.rs"]  # a hook regex matching "" or every probe "matches everything"
```

Rules (`LintFinding.rule`): `identifiers`, `string_match_user`, `executable_risk` (applied to every file in the entry dir except `rematch.json` and `EXPERIENCE.md`; markdown is linted with its `rematch:` frontmatter block removed; `hook.json` contributes only `message` + `pass_condition`); `trigger` (non-empty repo glob, stem/ext caps, non-empty trigger line present in the SKILL.md body or any `Use when…` line, non-empty `description`); `scripts` (every `scripts/<path>` referenced in markdown exists; no script executable unless listed in `Entry.approved_scripts`); `always_on`; `form_order` (trigger line, then classified headings, non-decreasing in `form.order`); `facts_as_of` (`facts/*.md`, and SKILL.md of `kind=fact`, need frontmatter `as_of`); `hook` (rule present, pass condition and message non-empty, regex compiles, does not match everything); `transfer` (`LintContext.transfer` and a domain lesson or fact). `LintReport.ok` iff no Error.

## 13. Ledger SQL (schema_version 1, `rematch_core::ledger`)

WAL, `synchronous=NORMAL`, `busy_timeout` 5 s. Migrations are an ordered list; `schema_version(version, created_at)` records each applied one; a ledger newer than the binary is refused. Blobs: `<ledger dir>/blobs/<sha256>` (atomic write via rename).

| table | columns (JSON = serde JSON text) | keys / indexes | notes |
| --- | --- | --- | --- |
| `sessions` | id, cwd, repo, branch, started_at, ended_at, family, provider, model, turns, tokens_in, tokens_out, tool_calls, tool_errors, skill_reads JSON, files_touched JSON, exts JSON, stems JSON, redirects JSON, flagged, counted, source, json (full `SessionRecord`), created_at | PK id; (branch, started_at); (counted, flagged) | reads deserialize `json`, then take `flagged`/`counted` from the columns and `redirects` from the `redirects` table. `counted` never goes back to 0 on re-upsert. Skill filter: `json_each(skill_reads)`. |
| `redirects` | id, session_id, seq, prelabel, class, confidence, text_head, deliverable_head, t, created_at | UNIQUE(session_id, seq) | classifier labels (`set_redirect_class`) survive a re-import that carries none. |
| `entries` | id, json (full `Entry`), created_at, updated_at | PK id | |
| `entry_status` | id, status, branch, name, predicate_hash, kind, family, updated_at | PK id; (status, branch); (name); (predicate_hash) | written in the same transaction as `entries`; `predicate_hash` = `predicate::predicate_hash`. |
| `counters` | entry_id, session_id, family, matched, recurrence, harm, tokens, turns, ts (session end), created_at | PK(entry_id, session_id); (family, ts) | one row per matched session ⇒ folding is idempotent. Aggregates and `harm_times` are computed (`ledger::aggregate`). |
| `proposals` | id (uuid v7), entry_id, session_id, blob (sha256 of proposal JSON), lint_ok, created_at | (entry_id); (session_id) | |
| `receipts` | id, entry_id, certificate_hash, activation, created_at | (entry_id) | approval certificates are blobs of `{entry, what, files:[[path, sha256]], at}`. |
| `admissions` | predicate_hash, day (UTC `YYYY-MM-DD`), entry_id, created_at | PK(predicate_hash, day, entry_id) | |
| `jobs` | id, kind, status, json, created_at, updated_at | (status) | `json` is the binary's job payload. |
| `outcomes` | id, session_id, ts, machine, family, repo, branch, kind, domain, subject_id, subject_json, claim, signature, params_json, evidence_json, result_json, links_json, provenance_json, status, json, created_at | PK id (latest status per id); exact (signature); temporal (subject_id, ts); (domain) | `subject_id` = `domain` or `domain#type:id,…` (sorted entities), `ledger::subject_id`. `latest`/`asof(ts)` = newest by ts (≤ ts). |
| `outcome_facets` | outcome_id, domain, key, value | composite (domain, key, value) | prefix seek: `domain = p OR domain LIKE 'p.%'` (via substr). |
| `minhash` | outcome_id, band, hash | PK(outcome_id, band); (band, hash) | `minhash_neighbors(bands, min_shared)` is a suggestion, never a gate. |

## 14. Binary module map and the omp process interface

`crates/rematch/src/`:
```
main.rs        clap dispatch → one JSON envelope on stdout (envelope.rs); exit codes 0/1/2/130; `--version` → {"version"}
app.rs         AppCtx{paths, ledger, now, cwd, text}: ensure_dirs, improver files (embedded; `$REMATCH_IMPROVER` overrides) copied on first run, ledger open; `new_entry`
envelope.rs    Envelope{ok, operation, data|error}; `--text` renders `data.lines` (else pretty JSON); `Cancelled` → 130, `Usage` → 2
config.rs      omp config.yml (agent dir + <repo>/.omp/config.yml overlay) → modelRoles lookup; role fallbacks (§0) + `rematch-solver`→default
proc.rs        bounded process execution with process-group kill (shared by editor, classifier, batch rollouts, verifiers)
omp.rs         omp invocation builders + JSON event stream parsing/verification (forge omp_runner contract)
editor.rs      model calls (editor, classifier, reflector) through omp or `--editor <cmd>`; tolerant reply parser; per-reflection CallBudget (≤ 3)
jobs.rs        detached jobs: `spawn_detached(ctx, kind, argv, payload) -> job_id` (setsid, stdio→jobs/<id>.log, `{job}` token + $REMATCH_JOB_ID), `update`, `status` (kill(pid,0) liveness), `LockFile`
library_cmd.rs library subcommands, off|on
reflect.rs     count + reflect --pending (§loop 1–8), editor/classifier calls, lint retry, stage, EXPERIENCE.md on decisions
create.rs      greenfield (sessions | task | description→draft), activate
improve.rs     brownfield (register, audit, split, unit-level propose, stage beside)
import_cmd.rs  import --history / --forge
digest.rs      session-start digest payload; capability-floor review list; transfer suggestions
doctor.rs      doctor
uninstall.rs   uninstall
batch/         Phase 2: tasks.rs (task defs), substrate.rs (worktree + overlay materialization), rollout.rs, verifier.rs, run.rs (k=1/width scheduler), worker.rs (run-worker)
scope/         Phase 3: ship.rs (team/fork PR), export.rs (work↔home export/import), stale.rs (drift), split.rs (per-family split), transfer.rs
```

### proc.rs (owned by Binary; used by everyone)
```rust
pub struct Bounded { pub stdout: String, pub stderr: String, pub code: Option<i32>, pub killed: bool, pub elapsed_ms: u64 }
/// Spawns `cmd` in its own process group (`process_group(0)`), stdin null, stdout/stderr piped (bounded buffers),
/// waits ≤ `timeout`; on timeout or cancel (SIGINT/SIGTERM to the binary ⇒ `Cancelled`, exit 130) fires SIGTERM to the group,
/// waits 2 s, SIGKILL to the group (also when a lingering grandchild keeps the pipes open after the leader exited).
pub fn run_bounded(cmd: std::process::Command, timeout: std::time::Duration) -> anyhow::Result<Bounded>;
pub fn run_bounded_input(cmd: Command, timeout: Duration, stdin: Option<&[u8]>) -> anyhow::Result<Bounded>;  // scripted editors
```

### omp.rs (owned by Binary)
```rust
pub fn omp_bin() -> PathBuf;                       // $OMP_BIN → PATH `omp`
pub struct NoToolsCall { pub model: ModelRef, pub system_prompt_file: PathBuf, pub prompt: String, pub timeout: Duration }
pub fn no_tools_argv(call: &NoToolsCall, cwd: &Path, no_tools_config: &Path) -> Vec<String>;   // §9 argv, exactly
pub struct ToolsCall { pub model: ModelRef, pub cwd: PathBuf, pub prompt: String, pub tools: Option<String>, pub max_time: Duration,
                       pub config_overlay: Option<PathBuf>, pub extra_args: Vec<String> }
pub fn tools_argv(call: &ToolsCall) -> Vec<String>;   // `--cwd --model --no-session --no-title --no-extensions --no-rules --no-lsp --approval-mode yolo --mode json --max-time -p -- prompt` (+ `--no-skills` unless overlay supplies customDirectories, + `--tools`)
pub fn child_env() -> Vec<(String, String)>;       // PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0
pub fn parse_events(stdout: &str) -> anyhow::Result<Vec<serde_json::Value>>;   // every non-empty line must be JSON
pub struct Verified { pub text: String, pub events: Vec<serde_json::Value>, pub usage: Option<serde_json::Value> }
pub fn verify_stream(events: Vec<Value>, expected: &ModelRef, no_tools: bool) -> anyhow::Result<Verified>;   // §9 rules
pub fn run_no_tools(call: &NoToolsCall, paths: &RematchPaths) -> anyhow::Result<Verified>;  // tmp cwd, run_bounded, verify
pub fn run_tools(call: &ToolsCall) -> anyhow::Result<Verified>;
```
`--editor <cmd>` (tests): `editor::call` bypasses `run_no_tools`, pipes the prompt to the command's stdin and reads stdout (see below).

### Binary behavior notes (Phase 1 + 1.5)

- Scripted model (`--editor <cmd>`): run with `sh -c`, prompt on stdin, env `REMATCH_ROLE=<role>` and `REMATCH_PROMPT=editor|classifier|reflector`; stdout is the reply. Without it, the role's model is resolved from config.yml and called through `run_no_tools`. The reflector uses the `rematch-editor` role with `improver/reflector.md`.
- Model calls: `count` makes ≤ 1 batched classifier call per pass (skipped when there are no unclassified non-mark redirects) and one reflector call per keep/retire (deterministic EXPERIENCE record when it fails). Each reflection's `CallBudget(3)` is charged for the classifier when its session was in that batch, then editor + one lint retry. A failed editor call keeps the pending file (`attempts` counter; dropped after 3).
- Detached jobs: the child of `spawn_detached` writes its final envelope into `jobs/<id>.json` (`status: done|failed`, `result`); a live status whose pid is gone reads as `lost`. `run-worker` manages its own record. `REMATCH_JOB_ID` is stripped from every `run_bounded` child.
- Ownership: an entry owns its directory iff it lives under REMATCH_ROOT or its SKILL.md carries a `rematch:` block with its id (`reflect::owns_dir`). Registered skills (`import --forge`, `improve`) and lessons are *foreign*: decisions, disable and pin are recorded in the ledger only (`reflect::record_status`), never moving or rewriting their files; their EXPERIENCE.md goes to `REMATCH_ROOT/brownfield/<id>/`; approve/decline/rollback refuse them.
- `import --forge`: skills register in branch `forge` with `path` = the forge skill dir; each `learnings/*.md` becomes a provisional `lesson` at `REMATCH_ROOT/lessons/forge/lesson-<slug>/` (`LESSON.md` copy + `rematch.json` with `evidence_sha256` = blob hash of the file, `source_path`). `match` omits lessons. `import --history` keeps an existing `source: trace` session row (richer) instead of overwriting it.
- `improve`: the audit counts every ledger session with the skill in `skill_reads` (traced and imported); < 5 ⇒ `{refused, traced_sessions, offer}`. Siblings share the base predicate (no pending-canary check), get `sibling_of` and stage at `staging/<branch>/<name>/` where branch = `--branch`, else the reading sessions' common repo, else `shared`.
- `create --from sessions`: request stems (name + description) and `--files` extensions vs. session stems/exts; a session matches with overlap ≥ min(2, |request stems|); ≥ 3 needed; up to 8 references. `activate --task` runs one reference rollout and attaches its moment as evidence before draft → provisional.
- `digest` lines: one per entry whose status changed since `REMATCH_ROOT/digest.last` (imported registrations summarized in one line), plus Scope review/stale/transfer lines; approvals = unapproved scripts or uncompiled hooks of owned provisional/kept entries, plus `export:<id>` items.
- `reflect` content-triage failures are logged (stderr + envelope) and the pending file deleted; no ledger proposal row.

## 15. Scope (Phase 3, `crates/rematch/src/scope/`)

CLI additions: `stale [--dry-run] [--recanary <entry>]`, `split [--review-days 7]`, `transfer promote`; `ship` adds `--push`, `--pr` (needs `--push` and `gh`), `--remote origin`; `library export` adds `--entry <id>… --approve | --approve-all`.
New `Entry` fields (serde default, omitted when empty): `fork_path`, `fork_repo`, `fork_commit`, `families_excluded`, `links` (`transfers_from`).

| Command / fn | Behavior |
| --- | --- |
| `ship --scope team` | Kept/provisional entries whose predicate content (`count_hit`, ignoring the one-repo glob) hit sessions in ≥ 2 distinct repos, or listed with `--entry`. Target = `--repo` or the originating session's repo. Lint must pass. A temporary worktree from HEAD gets branch `rematch/<name>-<last 8 id chars>` with `.omp/skills/<name>/` (no `rematch.json`/`EXPERIENCE.md`, mode 0644, `rematch.scope: team`), committed with an evidence body (counters, per family, matched sessions, repos, families, redirect class, predicted fix/regression, latest EXPERIENCE.md record). Receipt `activation: pr`. No network unless `--push`/`--pr`; never merges. |
| `ship --scope fork` | `--repo` required; entries with `fork_path`. Content = `fork/<basename>` in the package, else the SKILL.md body. Body adds `omp --version`, file sha256, base commit, before numbers (the predicate over sessions that started before the entry existed, `recurred`/`harmed` rules, per family) and after numbers (entry counters). Records `fork_repo` + `fork_commit` (base HEAD). |
| `library export` | `policy.json` `{environment: work|home, export_enabled}`; missing ⇒ home; work refuses unless `export_enabled`. Candidates: kept entries with `lesson_tag: invariant` or `manifest.scope: invariant`. Each is linted with `transfer: true`, `facts/` dropped, `rematch.json` stripped (provisional, no counters/evidence/approvals/path). Without approval the result lists `pending` with `git diff --no-index` diffs and `requires_approval: true`; `--entry <id> --approve` or `--approve-all` writes `<export>/<id>/` and commits it (the export dir is `git init`ed on first use). Receipt `activation: prompted`. `export::decline` records `{id: content sha}` in `export-declined.json` so the digest stops offering it until the content changes. |
| `library import-export` | Each `<dir>/<id>/rematch.json`: skip when already present (same id or `transfers_from` link), not invariant, or a near-duplicate (`dedup::route_duplicate`) of a live entry; branch = recorded branch when this library knows it, else `shared`; repo glob = that branch's latest session repo, else `**`; lint `transfer: true`; staged provisional with origin `Import{source: "export:<id>"}`. |
| `stale` | Kept/provisional, unpinned entries go `stale` when (a) a `facts/*.md` `source:` file's sha256 differs from its `source_sha256` frontmatter (written by `stale::snapshot_sources` after staging) or the file is gone, (b) the last 3 counted sessions in the branch (all branches for `shared`) all ran a family ≠ `family` and ∉ `verified_on`, (c) `git rev-parse HEAD` of `fork_repo` ≠ `fork_commit`. `--recanary` stages a new version (new id, version + 1, `predecessor`, family = the one now in use, fork commit cleared, sources re-snapshotted) and retires the stale one. |
| `split` | `counters::family_divergence` ⇒ sibling `<name>--<family slug>` (provisional, same branch, `family` = harmed, `sibling_of`, version + 1, original files + `units/family-note.md`, linted); the original gets `families_excluded += harmed` (informational: entries load regardless of family). Idempotent. `capability_review(window)`: families whose recurrence or harm rose on ≥ 2 entries in the last window vs the one before; review list only. |
| `transfer suggest / apply / promote` | Facets: `branches/<b>/domain.yml` `facets:` else distinct outcome facet keys of the branch (`docs/domain.yml.md`). Suggest kept invariant lessons for branches overlapping on ≥ 2 keys and not yet tried (same name, link, near-duplicate). Apply: provisional copy (new id, `links: transfers_from`, origin `Import{source: "transfer:<id>"}`, repo glob of the target branch, `facts/` dropped, approvals reset), lint `transfer: true`, rate limits except `NeedsMerge`. Promote: an entry kept in one branch plus kept (same name, link or near-duplicate) or a clean canary (matched ≥ 4, no recurrence/harm) in another ⇒ copy to `shared` (repo glob `**`) and keep it at once; `library` moves the branch version to predecessors (dormant). |

File ownership (all scope status changes and rematch.json writes): an entry's files are rematch's only when it is not `reflect::is_foreign` and its dir holds a `rematch.json` with its own id (`scope::owns_files`). User-owned entries (brownfield registrations in place, forge imports) get status changes in the ledger only (`reflect::record_status`); their dirs are never moved or written, and they are never `shared`-promotion sources. Entries with family `unknown/unknown` (imports) never go stale on the model-family rule.

## 16. Batch mode (Phase 2, `crates/rematch/src/batch/`)

New `Entry` field (serde default, omitted when false): `pending_next_run` — a batch winner staged by a `run` of its `Origin::Task`, decided by that task's next run.

Task (`tasks/<id>.json`): `{id, repo (absolute), prompt, verifier?, max_time_secs (default 600), tools?, seeds (default [1]), created_at}`. omp has no sampling seed: the seed names the workspace and is exported to the rollout as `REMATCH_SEED`.

Verifier specs (`run --verifier`, else the task's): `tests` (Cargo.toml → `cargo test`; pyproject/pytest.ini/setup.* → `python3 -m pytest -q`; package.json + bun lock → `bun test`), `parity:<produced>=<expected file | sha256>`, `receipt:<path>` (JSON `verdict: pass|fail`, optional `score`), `[script:]<cmd>` (`sh -c` in the worktree, exit 0 = pass, last stdout JSON line with `score` supplies the score). Verifiers run with env `REMATCH_TASK`, `REMATCH_WORKTREE`, `REMATCH_REPO`, capped at max(task cap, 60 s).

Rollout: `git worktree add --detach <variant>/wt HEAD` (plain `cp -R` for a non-git repo); the harness (kept/pinned entries + provisional entries `list_for_match` loads for the repo, candidates shadowing same-named entries) is written to `<variant>/skills/<name>/`; `--config <variant>/overlay.yml`:
```yaml
skills: {enabled: true, enableCodexUser: false, enableClaudeUser: false, enableClaudeProject: false, enablePiUser: false,
         enablePiProject: false, enableAgentsUser: false, enableAgentsProject: false, customDirectories: ["<variant>/skills"]}
memory: {backend: "off"}
memories: {enabled: false}
```
(keys from `omp config list`, omp 18.12.6; a probe showed only the custom-dir skill to the model). argv = `omp::tools_argv` with the `rematch-solver` role (→ `default`), env `child_env()` + `REMATCH_SEED` + `REMATCH=0`, bound = task cap + 60 s. stdout → `events.jsonl` (+ ledger blob); the stream must pass `verify_stream` (else verdict Fail "rollout invalid" without the verifier; a time-capped rollout is still judged). `events_to_trace` maps `session` → `session_start`, `message_end` user/assistant/toolResult → `input`/`turn_end`/`tool_result` (input recovered from the toolCall block), `agent_end`; the worktree path is rewritten to the repo.

Worker (`run-worker`): reference rollout (pending entries of the task loaded) → next-run rule: entries with `pending_next_run` and `Origin::Task{task}` go `kept` (+ `verified_on`, receipt) on a reference pass, `retired` on a fail → if the reference passes a score-less verifier the cycle stops (nothing can rank above it) → k invariant + k domain editor calls (`editor.md`; input = §9 shape with `redirect {class: correction, text: verifier detail}`, `task.prompt`, `verifier.verdict`, `prior_candidates` for i > 1; excerpt = around the longest consecutive tool-error stretch, else the trajectory tail), each linted and probed once → every (invariant, domain) pair composed: the best pair by one editor call with `scope: compose` (+ `candidates`, `behavioral_diff`) while the model-call budget lasts, the rest (and composer KEEP_BASE/failure/lint failure) by deterministic merge (union of files; domain laws that are simhash near-duplicates of an invariant law dropped; same-named entries merged) → each composed harness runs once; with ≥ 2 composed variants the best runs once more and must pass both → selection: (pass, score) desc, then tool errors, turns, lesson count, order; a winner must pass and beat the reference, else KEEP_BASE → winner lessons: a near-duplicate of a branch entry appends evidence to it; else one `rate::can_admit` per winner, staged provisional (origin Task, branch/predicate of the task repo, `lesson_tag` by scope, `pending_next_run: true`, receipt with the verdict). Budget at width k: rollouts = (k+1)² + [k>1] (4 at k=1, 10 at k=2), model calls = 2k + 1. Progress in `jobs/<id>.json` (`phase`, `rollouts_done`, `rollouts_budget`, `model_calls`, `verdicts`); receipt `runs/<task>/receipt.json` (+ blob) with every rollout summary, proposal, lint report and the decision; workspaces and worktrees are removed after the receipt (also on failure). One run per task at a time (`runs/<task>/.lock`).

