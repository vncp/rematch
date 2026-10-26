# omp-evolve — implementation contract

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
| Custom `modelRoles` keys are allowed in `~/.omp/agent/config.yml` and `<repo>/.omp/config.yml` | `evolve-editor` / `evolve-classifier` roles read by the binary from config.yml (no `modelRoles` API for externals). Fallbacks: editor→`slow`→`default`; classifier→`smol`→`tiny`→`default`. |
| Native extension roots: `~/.omp/agent/extensions/{*.ts, <dir>/index.ts}`; a subdir's non-index `*.ts` is **not** discovered | `~/.omp/agent/extensions/evolve-hooks/<id>.ts` files are inert to omp; the main extension `tool_call` handler dynamically imports them (the spec's "run approved compiled verifiers"). |
| `omp plugin install <local-path>` links the dir into `~/.omp/plugins/node_modules/<pkg.name>` and enables it in `omp-plugins.lock.json`; `package.json#omp.extensions` lists entries | Install: `omp plugin link <root>/extension` (or `install`). Binary on PATH via `~/.local/bin/omp-evolve` shim written by `scripts/install.sh`. |
| `omp anonymize` writes a shareable redacted copy of a session | `tests/goldens/` holds anonymized real sessions for the importer tests. |
| Credentials, `agent.db`, `models.db` live in the agent dir | Batch isolation does **not** swap `PI_CODING_AGENT_DIR` (would lose auth). Isolation = fresh git worktree + `--config <overlay.yml>` (all skill providers off, `skills.customDirectories=[candidate]`, memory off) + `--no-extensions --no-rules --no-session`. |

## 1. Topology

```
extension/index.ts  ──pi.exec JSON envelope──▶  omp-evolve (Rust, static)
   │ writes ~/.omp/evolve/traces/<session>.jsonl        │ owns ledger.sqlite, staging/, library, improver, jobs
   │ writes ~/.omp/evolve/pending/<session>.json         │ spawns `omp -p --no-tools --mode json` (editor, classifier)
   │ factory: `match --cwd` → skills.customDirectories   │ writes staging/<branch>/<name>/ and ~/.omp/agent/skills/<name>/
   └ tool_call: runs ~/.omp/agent/extensions/evolve-hooks/<id>.ts
```

Root `EVOLVE_ROOT` = `$OMP_EVOLVE_ROOT` or `~/.omp/evolve`. Agent dir = `$PI_CODING_AGENT_DIR` or `~/.omp/agent`.
`OMP_EVOLVE=0` or `EVOLVE_ROOT/off` file ⇒ extension registers nothing but `/evolve on` and the `omp_evolve` tool; binary refuses reflect/stage.

### Files under EVOLVE_ROOT

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
runs/<task>/<seed>/               batch workspaces (deleted after receipt)
tasks/<task_id>.json              batch task definitions
branches/<branch>/domain.yml      facet vocabulary per branch
export/                           git-tracked work↔home export of invariant entries
off                               kill switch marker
```

## 2. CLI (binary `omp-evolve`)

Every subcommand prints exactly one JSON line on stdout:
`{"ok":true,"operation":"<op>","data":{...}}` (exit 0) or `{"ok":false,"operation":"<op>","error":"..."}` (exit 1; 2 usage; 130 cancelled).
Human-readable output only with `--text`. Logs go to stderr.

```
doctor                                     omp binary+version, roles resolved (provider/model), extension linked?, ledger schema version, root paths, off?
import --history [--sessions <dir>] [--forge <path>]
reflect --pending [--session <id>] [--editor <cmd>]      idempotent per pending file; --editor overrides the editor channel (tests)
match --cwd <dir> [--inputs <json>]        {branch, repo, skill_dirs:[...], entries:[{id,name,status,predicate_hit}], hooks:[path...], off:bool}
digest --cwd <dir>                         {lines:[...], approvals:[{id,name,kind,diff,reason}], review:[...capability floor...], transfers:[...]}
record --session <id> --event <json>       (optional path used by the extension for marks; the trace file is the primary input)
create <name> --from sessions|task|description [--task <id>] [--branch <b>] [--description <text>] [--files <p>...]
improve <skill> [--split-only] [--branch <b>]
activate <name> [--task <id> | --unverified]
run --task <id> --verifier <cmd> [--width k] [--seed n]  → {job_id}
run-worker --job <id>                      internal, detached
status [job]
library ls | why [<session_id>] | disable <id> | rollback <skill> | pin <id> | unpin <id> | approve <id> | decline <id> | export [--to <dir>] | import-export [--from <dir>]
task add <id> --repo <path> --prompt <text> [--verifier <cmd>] [--max-time 10m] [--tools ...]
transfer suggest | transfer apply <lesson_id> --branch <b>
ship --scope team|fork [--repo <path>] [--entry <id>...]
count [--session <id>]                     fold finished traces into counters and decide (also run by reflect --pending)
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

Pre-classification (extension, no model): `interrupt` = input while `!ctx.isIdle()`; `revert` = text matches `/\b(revert|undo|roll ?back|put it back|git checkout --|git restore)\b/i`; `reprompt` = Jaccard(tokens(text), tokens(previous input)) ≥ 0.6; `negation` = `/^(no|nope|don't|do not|stop|not that|wrong|that's not|i said|actually|instead)\b/i`.
Session is flagged when any of: ≥1 prelabel ≠ none; `is_error` tool results ≥ 3 in any window of 6 consecutive tool results (tool-error cluster); a `mark`. Flag = write `pending/<session_id>.json`.

## 4. Core types (`evolve-core/src/types.rs`, authoritative in Rust)

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
<dir>/SKILL.md            frontmatter: name, description, evolve: {id, status, branch, family, version, predicate_hash}
<dir>/evolve.json         serde of Entry + manifest + predicate (the ledger row mirrors it; evolve.json is authoritative for files, ledger for counters)
<dir>/units/*.md  facts/*.md (each fact has `as_of:`)  scripts/* (mode 0644 until approved; 0755 after)
<dir>/hook.json           kind=Hook only: {tool, field, regex, message, pass_condition}
<dir>/EXPERIENCE.md       written by the reflector after keep/retire (read-only to the system)
```

## 5. Ledger (SQLite, `rusqlite` bundled)

Tables (all with `created_at` ms): `schema_version`; `sessions(id PK, cwd, repo, branch, started_at, ended_at, family, provider, model, turns, tokens_in, tokens_out, tool_calls, tool_errors, skill_reads JSON, files_touched JSON, exts JSON, stems JSON, redirects JSON, flagged, counted, source 'trace'|'import')`; `redirects(id, session_id, seq, prelabel, class, confidence, text_head, deliverable_head)`; `entries(id PK, json)` + `entry_status(id, status, branch, name, predicate_hash, kind, family)`; `counters(entry_id, session_id, family, matched, recurrence, harm, tokens, turns, PRIMARY KEY(entry_id, session_id))`; `proposals(id, entry_id, session_id, blob, lint_ok, created_at)`; `receipts(id, entry_id, certificate_hash, activation)`; `admissions(predicate_hash, day, entry_id)`; `jobs(id, kind, status, json)`; `outcomes(id, session_id, ts, machine, family, repo, branch, kind, domain, subject_json, claim, signature, params_json, evidence_json, result_json, links_json, provenance_json, status)` with indexes `exact(signature)`, `composite` via `outcome_facets(outcome_id, domain, key, value)` indexed `(domain,key,value)`, `temporal(subject_id, ts)`; `minhash(outcome_id, band, hash)`.

## 6. Predicate semantics (deterministic, same everywhere)

Derivation (binary, on stage): `repo_glob` = the originating session's repo path (exact) or `**` if no repo; `file_exts` = extensions of files read/edited in the evidence window (≤8); `stems` = up to 10 Porter-stemmed, stop-worded, lowercase tokens from the originating user turn + basenames of touched files, ranked by tf-idf against the ledger's session corpus.
Load-time match (`match --cwd`): glob matches repo (or cwd) ⇒ branch dir contributed when ≥1 provisional entry in that branch matches.
Count-time match (binary, from a finished trace): glob hit AND (`file_exts` empty OR overlap ≥1) AND (`stems` empty OR overlap ≥ min(2, stems.len())). Inputs = first 5 `input` texts + all `tool_result.path` values.

## 7. Counters and decisions (SPEC §loop 7–8, verbatim)

Per matched session (predicate hit while the entry was in `loaded_entries`): `recurrence += 1` if a redirect of the entry's `redirect_class` occurred (classifier output, or prelabel ∈ {revert, negation, mark} when class unknown); `harm += 1` if a new tool-error cluster, an `interrupt`, or a user `disable` happened. Tokens/turns recorded.
Decide: `matched ≥ 4 && recurrence == 0 && harm == 0 → Keep`; `recurrence ≥ 2 || harm ≥ 2 → Retire`; `matched ≥ 12 → Dormant`; `provisional for 30 days && matched < 3 → Dormant`; `Kept && harm ≥ 2 within 30 days → Retire`; else `Continue`. Per-family breakdown kept; if one family shows harm ≥ 1 and another has matched ≥ 2 with zero harm/recurrence, the entry is split (Phase 3): the harmed family gets a sibling proposal.

## 8. Rate limits

Per predicate_hash: ≤1 admission per UTC day; no reflection while a provisional entry with the same predicate_hash exists (pending canary). Per branch: ≤10 provisional; 5 kept general + 5 kept per branch before a new candidate must merge/revise (near-duplicate by simhash(normalized text) Hamming ≤ 6 or identical script body ⇒ append evidence to existing entry). ≤3 model calls per reflection (classifier batch, editor, one lint-retry).

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

Factory (async): read env/off; resolve binary (`$OMP_EVOLVE_BIN` → `<ext>/../target/release/omp-evolve` → `<ext>/../target/debug/omp-evolve` → PATH); `await pi.exec(bin, ["match","--cwd",process.cwd()])`; override `skills.customDirectories` with `[...configured, ...skill_dirs]`; remember `entries`/`hooks`; register tool `omp_evolve {operation, arguments[]}` (approval: read for doctor/status/library/digest/match/why, exec otherwise), command `/evolve`, shortcut `alt+e` (digest), handlers §3. `session_start`: open trace, write `session_start`, spawn `reflect --pending` detached (`Bun.spawn` with `stdio ignore`, `unref`), run `digest` and present lines via `ctx.ui.notify`, and each approval via `ctx.ui.confirm(title, diff)` → `library approve|decline <id>`. `tool_call`: evaluate loaded hook modules (`default export {id, tool, test(input)=>string|null}`); block with the rule's message, record `tool_blocked`. `/evolve <sub>`: `why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|ship|transfer` map 1:1 onto the binary; long ones return a job id.

## 11. Deviations from SPEC (all forced by §0)

1. `resources_discover` → factory-time `skills.customDirectories` override (same semantics, per process).
2. Batch isolation uses a `--config` overlay + fresh worktree instead of a private `PI_CODING_AGENT_DIR` (auth lives in the agent dir).
3. `omp plugin install @<org>/omp-evolve` → `scripts/install.sh` (`cargo build --release`, `omp plugin link extension/`, PATH shim). No registry publish.
4. "Related thing: embeddings, exploratory only" is not implemented: the binary holds no provider credentials; exact signature + MinHash are implemented.
