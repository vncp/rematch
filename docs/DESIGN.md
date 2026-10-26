# rematch — implementation contract

Source of truth for every slice. The build spec is `docs/SPEC.md`; this file records
the verified omp facts, the deviations forced by them, and the exact interfaces the
crates, the extension and the improver share. When SPEC and DESIGN disagree, DESIGN
wins (it exists only where a technical constraint was verified).

## 0. Verified omp facts (omp v18.12.6; `omp://extensions.md`, `omp://skills.md`, probes)

| Fact | Consequence |
| --- | --- |
| `resources_discover` is implemented in `ExtensionRunner` but **no `AgentSession` callsite emits it** | Cannot contribute `skillPaths` at runtime. Replaced (see §1). |
| `lookup("skills.customDirectories").override(pi.pi.settings, [...])` executed **inside the extension factory** lands before `loadSkills()`: probe showed the custom-dir skill in `before_agent_start.systemPrompt` | Load mechanism: factory has the binary sync the shared skills dir for `cwd` (`skills sync`), then overrides the setting additively (`[...configured, REMATCH_ROOT/skills]`). Decision holds for the process (spec already caveats). |
| Skills of the same name are deduplicated by realpath: identical files collapse into one (`omp://skills.md`) | A kept entry reachable natively (`~/.omp/agent/skills/<name>`) and through its link in the shared dir `REMATCH_ROOT/skills/` loads once (§16 "Shared skills"). |
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
| Task agents are discovered from `<cwd>/.omp/agents/*.md` (project), `~/.omp/agent/agents/*.md` (user) and `<extension-root>/agents/*.md` of every enabled plugin, in that precedence, first `name` wins; frontmatter needs `name` + `description`, extra keys are ignored (`omp://task-agent-discovery.md`). No setting adds an agent dir at runtime | Provisional agents load through the plugin root: `extension/agents` is a symlink (made by `install.sh`, gitignored) to `REMATCH_ROOT/agents/`, which holds one symlink per provisional agent. They are visible in **every** session while provisional (counted only where the predicate's glob hits); kept agents are copied to `~/.omp/agent/agents/<name>.md`. A user or project agent of the same name shadows a provisional one. |
| Agent dir resolution (`omp://config-usage.md#profiles`, `omp://extension-loading.md`, `@oh-my-pi/pi-utils/src/dirs.ts`): `PI_CONFIG_DIR` replaces `~/.omp`; `OMP_PROFILE` (when defined, including empty/`default` which selects default and suppresses `PI_PROFILE`) else `PI_PROFILE` selects `~/.omp/profiles/<name>/agent` (ignoring `PI_CODING_AGENT_DIR`); in the default profile `PI_CODING_AGENT_DIR` overrides `~/.omp/agent` (unless it equals a profile-derived agent dir inherited from `PI_PROFILE`). | `RematchPaths::resolve()` follows omp's exact precedence; `REMATCH_ROOT` stays `$REMATCH_ROOT` or `~/.rematch` (shared across profiles). `import --history` without `--sessions` imports the default agent dir plus every `<config_root>/profiles/*/agent/sessions` except profiles in `--skip-profile <name>` or `policy.json` `{"skip_profiles":[...]}`. |
| `context` event (`pi-coding-agent/src/extensibility/shared-events.ts` `ContextEvent {messages}`; `extensions/runner.ts` `emitContext`): runs before every provider request on a `structuredClone` of the messages; each handler's returned `{messages}` feeds the next; the persisted history is untouched; each handler is bounded by a 30 s timeout (`EXTENSION_HANDLER_TIMEOUT_MS`). Message shapes (`pi-ai/src/types.ts`): `user {content: string \| blocks, synthetic?}`, `toolResult {toolCallId, toolName, content: (text\|image)[], isError}`; assistant tool calls are `{type:"toolCall", id, name, arguments}` blocks. A `CustomMessage {role:"custom", customType, content, display, details?, timestamp}` (`pi-agent-core/src/compaction/messages.ts`) is sent to the provider as a `developer` message (`convertMessageToLlm`, reached from `session/messages.ts` `convertToLlm`) | `truncate` and `retrieve` hooks rewrite only the outgoing copy: truncation replaces old `toolResult` text, retrieval appends one hidden `CustomMessage` (`customType: "com.vncp.rematch.retrieval"`). |

## 1. Topology

```
extension/index.ts  ──pi.exec JSON envelope──▶  rematch (Rust, static)
   │ writes ~/.rematch/traces/<session>.jsonl           │ owns ledger.sqlite, staging/, library, improver, jobs
   │ writes ~/.rematch/pending/<session>.json            │ spawns `omp -p --no-tools --mode json` (editor, classifier)
   │ factory: `skills sync --cwd` → customDirectories    │ writes staging/<branch>/<name>/ and ~/.omp/agent/skills/<name>/
   │   += ~/.rematch/skills; `match --cwd` → hooks       │ links ~/.rematch/skills/<name> → staging or kept entry dir
   │ extension/agents → ~/.rematch/agents (task agents)  │ links ~/.rematch/agents/<name>.md; copies kept agents to ~/.omp/agent/agents/<name>.md
   └ tool_call / context: runs ~/.omp/agent/extensions/rematch-hooks/<id>.ts (block, stuck / truncate, retrieve → `retrieve --query`)
```

Root `REMATCH_ROOT` = `$REMATCH_ROOT` or `~/.rematch` (shared across profiles). Config root = `~/$PI_CONFIG_DIR` or `~/.omp`. Agent dir = `<config_root>/profiles/<name>/agent` when `OMP_PROFILE` (else `PI_PROFILE`) names a profile ≠ `default`, else `$PI_CODING_AGENT_DIR` (default profile only), else `<config_root>/agent`.
`REMATCH=0` or `REMATCH_ROOT/off` file ⇒ extension registers nothing but `/rematch on` and the `rematch` tool; binary refuses reflect/stage.

### Files under REMATCH_ROOT

```
traces/<session_id>.jsonl         extension / adapter trace (schema §3); Claude Code sessions `cc:<uuid>`, Codex `codex:<id>` (§18)
pending/<session_id>.json         {session_id, flagged_at, reasons:[...], trace}   written at shutdown; deleted by reflect
skills/<name>                     symlink → staging/<branch>/<name>/ or the kept dir, one per loadable entry: the one skill dir omp, Claude Code and Codex load (§16 "Shared skills")
ledger.sqlite  blobs/<sha256>     ledger (schema §5) + content-addressed blobs
staging/<branch>/<name>/          provisional entries (loaded through skills/ once a synced cwd matched them; agents via agents/)
agents/<name>.md                  symlink → staging/<branch>/<name>/AGENT.md per provisional agent; `extension/agents` links to this dir, so omp discovers them from the plugin root
kept-agents/<name>/               entry dir of a kept agent (AGENT.md, rematch.json, EXPERIENCE.md); omp loads the copy ~/.omp/agent/agents/<name>.md
drafts/<name>/                    draft entries (never loaded)
archive/<status>/<id>/            retired|dormant|stale entries (never loaded, never deleted)
predecessors/<name>/<version>/    copies of replaced kept entries (rollback); agents: predecessors/.agents/<name>/<version>/ (version 0 = the user's own ~/.omp/agent/agents/<name>.md)
improver/{editor.md,composer.md,reflector.md,classifier.md,lint.toml,omp-no-tools.yml}   shipped in the binary; written on first run and refreshed on upgrade only if the installed copy still matches the previously shipped hash (`.shipped/<name>`); EXPERIENCE.md files live next to entries
jobs/<job_id>.json  jobs/<job_id>.log      detached job state
runs/<task>/<seed>/<variant>/     batch workspaces {wt/, skills/, overlay.yml, events.jsonl} (deleted after receipt)
runs/<task>/receipt.json          last batch receipt;  runs/<task>/traces/<session>.jsonl  reference traces for `create --from task`
tasks/<task_id>.json              batch task definitions
branches/<branch>/domain.yml      facet vocabulary per branch
export/                           git-tracked work↔home export of invariant entries
builtins.version  .builtins.lock  hash of the shipped built-in entries (§10) and the install lock
off                               kill switch marker
```

## 2. CLI (binary `rematch`)

Every subcommand prints exactly one JSON line on stdout:
`{"ok":true,"operation":"<op>","data":{...}}` (exit 0) or `{"ok":false,"operation":"<op>","error":"..."}` (exit 1; 2 usage; 130 cancelled).
Human-readable output only with `--text`. Logs go to stderr.

```
doctor                                     omp binary+version, roles resolved (provider/model), extension linked?, ledger schema version, root + agent_dir + profiles (active/existing/skipped), off?
import --history [--harness omp|claude-code|codex|all] [--sessions <dir>] [--skip-profile <name>...] [--forge <path> [--migrate]]   --harness default all (each harness's default dir that exists); --sessions = the selected harness's dir (with all: an omp dir); clean imported Codex sessions that loaded entries get their synthesized trace written (count folds them)
reflect --pending [--session <id>] [--editor <cmd>] [--detach] [--history [--since YYYY-MM-DD] [--limit n]]   idempotent per pending file (lock `pending/.lock`); --editor overrides every model call (tests); --detach → {job_id}; --history first hands flagged imported sessions to the loop (≤ n pending files per run, default 10)
match --cwd <dir> [--inputs <json>]       {branch, repo, skill_dirs:[...], entries:[{id,name,kind,status,predicate_hit}], hooks:[path...], off:bool}   (provisional agents are listed in every session; they add no skill dir; skill_dirs is kept for compatibility, harnesses load skills/)
skills sync --cwd <dir> [--harness omp|claude-code|codex]   {added, pruned, total, dirs, links:{name: target}, untouched, off, cwd, repo, harness, migrated, lines} (+ plugin_link with claude-code and $CLAUDE_PLUGIN_ROOT, reached_from with codex): syncs REMATCH_ROOT/skills for the cwd (§16 "Shared skills")
digest --cwd <dir>                         {lines:[...], approvals:[{id,name,kind,diff,reason}], review:[...capability floor...], transfers:[...]}
record --session <id> --event <json>       accepts `{"ev":"mark","class":…}` only (= mark); every other event belongs in the trace file
trace-event --harness claude-code [--session <id>] [--hook-output]   one Claude Code hook payload on stdin → §3 records in traces/cc:<uuid>.jsonl (§18.1); --hook-output prints the hook reply instead of the envelope and exits 0 on any error
hook-check --harness claude-code [--session <id>] [--hook-output]    one PreToolUse payload on stdin → {block, hook_id?, reason?}: approved block/stuck hooks for the payload cwd; a block is traced as tool_blocked and, with --hook-output, exits 2 with the reason on stderr
create <name> --from sessions|task|description [--task <id>] [--branch <b>] [--description <text>] [--files <p>...] [--editor <cmd>]
improve <skill> [--split-only] [--branch <b>] [--editor <cmd>]
activate <name> [--task <id> | --unverified]
run --task <id> --verifier <cmd> [--width k] [--seed n]  → {job_id}
run-worker --job <id>                      internal, detached
status [job]
library ls | why [<session_id>] | disable <id> | rollback <name> | pin <id> | unpin <id> | approve <id> | decline <id> | set-fork-path <id> <repo-relative path> [--repo <fork>] | export [--to <dir>] | import-export [--from <dir>]
task add <id> --repo <path> --prompt <text> [--verifier <cmd>] [--max-time 10m] [--tools ...]
transfer suggest | transfer apply <lesson_id> --branch <b>
ship --scope team|fork [--repo <path>] [--entry <id>...]   (fork: --repo defaults to the entry's fork_repo)
count [--session <id>] [--editor <cmd>]    fold finished traces into counters and decide (also run by reflect --pending)
mark --session <id> --class correction|preference|scope
anchor replay <entry>                      one anchor replay for the kept entry <entry> would replace (§7); reports {kept, anchor:{outcome: replayed|no_verifier, pass, receipt}}, changes no status, writes no receipt
retrieve --query <text> [--budget 6000] [--min-overlap 2] [--entries <id,…>]   {units:[{entry_id,name,path,title,text,overlap}], used_bytes, query_stems}: units of the package entries loaded for --cwd (match entries with a predicate hit, plus kept/pinned), restricted to --entries (§10 retrieve)
builtins ls | reinstall [<name>…]         built-in context entries (§10): list with status/approval/counters; re-stage the ones that are not live
off | on
uninstall [--yes]
```

`reflect --pending` performs, in order: count (fold all finished traces not yet counted → counters → decide → EXPERIENCE.md), then one reflection per pending file respecting rate limits. With `--history` it first stages, newest first, every imported session (`source: import`) that is flagged, has no trace file and no `history_reflected_at` marker: `traces/<sid>.jsonl` synthesized from its omp session file (§3, `mode: "import"`) and `pending/<sid>.json` (`source: "import"`); subagent files and sessions the extension's rule does not flag are marked and skipped. The envelope's `history.sessions` gives each session's outcome (`staged`, `keep_base`, `rate_limited`, `skipped` + reason, …).

## 3. Trace schema (extension → binary)

`traces/<session_id>.jsonl`, one JSON object per line, written append-only by the extension:
`{"seq": <u32>, "t": <unix ms>, "ev": "<name>", ...}`. The extension never reads it back.

| ev | fields |
| --- | --- |
| `session_start` | `session_id, cwd, repo (git toplevel or null), branch_hint (basename), model {provider,id}, mode, agent {kind,name}, omp_version, loaded_entries [id...] (match entries with a predicate hit, minus `retrieve` hooks, which enter through `retrieval`), hooks_loaded [id...], staging_dirs [...]` |
| `input` | `text (≤16 KB), source, idle (bool), after_deliverable (last assistant text ≤2 KB or null), prelabel ∈ {"none","interrupt","revert","reprompt","negation","mark:<class>"}, skills_invoked [names from /skill: tokens]` |
| `tool_result` | `tool, call_id, input (JSON ≤8 KB, keys kept, long strings truncated with "…[+N]"), args_hash (sha256 of canonical JSON of full input), result_bytes, is_error, error_class (null | "error"), content_head (first 1 KB of text content), skill_read (name or null), path (input.path/file_path when present)` |
| `tool_blocked` | `tool, call_id, hook_id, reason` (a `block` or `stuck` hook); `args_hash` when written by the Claude Code adapter, whose `stuck` window is read back from the trace |
| `turn_end` | `model {provider,id}, usage {input,output,cacheRead,cacheWrite,totalTokens,cost}, stop_reason, text_head (≤2 KB)` |
| `agent_end` | `turns, is_terminal, yielded` |
| `model_change` | `from {provider,id} , to {provider,id}, reason` (from `retry_fallback_applied` or turn-to-turn change) |
| `mark` | `class ∈ correction|preference|scope` |
| `retrieval` | `entry_ids [retrieve hook ids], units [{entry_id, path (relative to the entry dir), bytes}], bytes` — written when a `retrieve` hook appended a different unit set; `trace::fold_trace` adds `entry_ids` and every `units[].entry_id` to the session's `loaded_entries`, so counting attributes the added context to the hook and to the packages it came from |
| `session_shutdown` | `reasons [...], flagged (bool)` |

Pre-classification (extension, no model; regex constants live in `rematch-core/src/redirect.rs` and are copied verbatim into the extension): `mark` = `/^\/rematch\s+mark\s+(correction|preference|scope)\b/i` (the Claude Code adapter first spells its `/rematch-mark <class>` and `/rematch:rematch-mark <class>` as `/rematch mark <class>`, §18.1); `interrupt` = input while the agent loop is running (between `agent_start` and `agent_end` with `willContinue`; `ctx.isIdle()` is also false while a finished turn settles and over-counted takeovers in live use); `revert` = `/\b(?:revert|undo|roll ?back|put it back|git restore)\b|\bgit checkout --(?:\s|$)/i`; `negation` = `/^(?:no|nope|don't|do not|stop|not that|wrong|that's (?:not|wrong)|that is wrong|this is wrong|incorrect|you didn't|you did not|not what i|i said|actually|instead)\b/i` (leading whitespace trimmed); `reprompt` = Jaccard(tokens(text), tokens(previous input)) ≥ 0.6 with tokens `[a-z0-9]+` on lowercased text and at least 6 tokens in the new input ("four" after "three" is not a redo). Precedence: mark > interrupt > revert > negation > reprompt > none.
Session is flagged when any of: ≥1 prelabel ≠ none; `is_error` tool results ≥ 3 in any window of 6 consecutive tool results (tool-error cluster); a `mark` (`trace::flag_reasons`). Flag = write `pending/<session_id>.json`.

Imported sessions get the same trace after the fact (`rematch_core::import::trace_records_of`, the active branch of the omp JSONL): `session_start` (header id/cwd, repo by walking up to `.git`, model of the first assistant message, `mode: "import"`, agent `main` or `sub` for subagent files, `omp_version` from `runtime.version`, no loaded entries/hooks/staging dirs); each user message not attributed to an agent → `input` with the rules above (idle unless the previous assistant stopped `aborted`; slash/`!`/`%` lines other than mark and `/skill:` dropped as live), preceded by `agent_end` for the run it ends; each `toolResult` → `tool_result` paired with its `toolCall` by id; each assistant message → `turn_end` after its tool results, preceded by `model_change` when provider/model changed; then `agent_end` and `session_shutdown` with the flag reasons. Not recoverable from omp JSONL: `input.source` (empty), `branch_hint` beyond the repo basename, `loaded_entries`/`hooks_loaded`/`staging_dirs` (empty), `tool_blocked`, `mark`, retry-fallback `model_change` reasons, `agent_end.is_terminal` (always true). Redirect and error-cluster seqs of an imported ledger row address this synthesized trace.

Claude Code and Codex sessions get the same schema: live from the Claude Code plugin's hooks (`trace-event`, §18.1) and after the fact from their transcripts (`rematch_core::import_claude::trace_records_of`, `import_codex::trace_records_of`, §18).

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
<dir>/AGENT.md            kind=Agent instead of SKILL.md: frontmatter name (= entry name), description, optional model/tools/…, rematch: {…}; body = the subagent's system prompt
<dir>/rematch.json        serde of Entry + manifest + predicate (the ledger row mirrors it; rematch.json is authoritative for files, ledger for counters)
<dir>/units/*.md  facts/*.md (each fact has `as_of:`)  scripts/* (mode 0644 until approved; 0755 after)
<dir>/hook.json           kind=Hook only: HookRule {mode (block|truncate|stuck|retrieve, default block), tool (`*` = any), field + regex (block), message, pass_condition, max_age_turns + keep_chars (truncate), repeats (stuck), budget_bytes + min_overlap (retrieve)}; unset mode parameters take the defaults 6, 1200, 3, 6000, 2 (`HookRule::params`)
<dir>/AGENTS.fragment.md  optional: text for `<repo>/AGENTS.md`, reaches the repo only through `ship --scope team` (never loaded locally)
<dir>/fork/<basename>     optional: the text `ship --scope fork` writes to `<fork>/<fork_path>`
<dir>/EXPERIENCE.md       written by the reflector after keep/retire (read-only to the system)
```

## 5. Ledger (SQLite, `rusqlite` bundled)

Tables (all with `created_at` ms): `schema_version`; `sessions(id PK, cwd, repo, branch, started_at, ended_at, family, provider, model, turns, tokens_in, tokens_out, tool_calls, tool_errors, skill_reads JSON, files_touched JSON, exts JSON, stems JSON, redirects JSON, flagged, counted, source 'trace'|'import')`; `redirects(id, session_id, seq, prelabel, class, confidence, text_head, deliverable_head)`; `entries(id PK, json)` + `entry_status(id, status, branch, name, predicate_hash, kind, family)`; `counters(entry_id, session_id, family, matched, recurrence, harm, tokens, turns, PRIMARY KEY(entry_id, session_id))`; `proposals(id, entry_id, session_id, blob, lint_ok, created_at)`; `receipts(id, entry_id, certificate_hash, activation)`; `admissions(predicate_hash, day, entry_id)`; `jobs(id, kind, status, json)`; `outcomes(id, session_id, ts, machine, family, repo, branch, kind, domain, subject_json, claim, signature, params_json, evidence_json, result_json, links_json, provenance_json, status)` with indexes `exact(signature)`, `composite` via `outcome_facets(outcome_id, domain, key, value)` indexed `(domain,key,value)`, `temporal(subject_id, ts)`; `minhash(outcome_id, band, hash)`.

## 6. Predicate semantics (deterministic, same everywhere)

Derivation (binary, on stage): `repo_glob` = the originating session's repo path (exact, as recorded by the extension from `git rev-parse --show-toplevel`) or `**` if no repo; `file_exts` = extensions of files read/edited in the evidence window (≤8); `stems` = up to 10 Porter-stemmed, stop-worded, lowercase tokens ranked by tf-idf against the ledger's session corpus over a weighted document: the originating task turn (the last user input before the redirect) ×2 + basenames of touched files ×2 + the redirect turn ×1. The redirect's own wording ("that's wrong", "every") must not crowd out the task's domain terms, or later sessions on the same task never count (observed on the first real run; fixed in `reflect.rs`). The editor's `entry.stems` are used only when derivation yields nothing: the pre-editor predicate hash is what the rate limits key on.
Load-time match (`match --cwd`): glob matches repo (or cwd) ⇒ branch dir contributed when ≥1 provisional non-agent, active entry in that branch matches. A `kind: hook` entry is active only while its approved module `rematch-hooks/<id>.ts` exists (`library::hook_active`): before approval it contributes no dir and is listed with `predicate_hit: false`, so it never counts. Provisional agents are listed in every session (omp discovers them from the plugin root regardless of the cwd) with `predicate_hit` = their glob hit, so the trace's `loaded_entries` carries them only where they would count. Both the given path and its `canonicalize()`d form are tried (macOS `/tmp` → `/private/tmp`), on either side.
Triage (`trace::triage`) has two independent axes: *kind* process|content decides whether to propose (content = a corrected prose answer with no tool activity and no process words; anything the agent produced with tools, tool-error clusters, takeovers, reverts and procedural text are process); *scope* domain|invariant picks the editor (domain by default; invariant for error clusters, takeovers, reverts, or text about the agent's general way of working, `INVARIANT_TEXT_PATTERN`). After classification, a session whose only redirects are scope|none and that has no cluster/takeover/mark carries no signal: no editor call.
Count-time match (binary, from a finished trace): glob hit AND (`file_exts` empty OR overlap ≥1) AND (`stems` empty OR overlap ≥ min(2, stems.len())). Inputs = first 5 `input` texts + all `tool_result.path` values.

## 7. Counters and decisions (SPEC §loop 7–8, verbatim)

Per matched session (predicate hit while the entry was in `loaded_entries`): `recurrence += 1` if a redirect of the entry's `redirect_class` occurred (classifier output, or prelabel ∈ {revert, negation, mark} when class unknown); `harm += 1` if a new tool-error cluster, an `interrupt`, or a user `disable` happened. Tokens/turns recorded.
Decide: `matched ≥ 4 && recurrence == 0 && harm == 0 → Keep`; `recurrence ≥ 2 || harm ≥ 2 → Retire`; `matched ≥ 12 → Dormant`; `provisional for 30 days && matched < 3 → Dormant`; `Kept && harm ≥ 2 within 30 days → Retire`; else `Continue`. Per-family breakdown kept; if one family shows harm ≥ 1 and another has matched ≥ 2 with zero harm/recurrence, the entry is split (Phase 3): the harmed family gets a sibling proposal.
Implementation notes (`counters.rs`): each matched session contributes 0/1 recurrence and 0/1 harm (one row per (entry, session)). Prelabel mapping when the classifier gave no class: `revert|negation|mark` → correction, `mark:<class>` → that class; `interrupt|reprompt|none` → no class; a classifier `none` means no class. An entry without a `redirect_class` (motivated by an error cluster or takeover, and the built-ins of §10) recurs on a correction or preference redirect; a scope redirect (the user widened or changed the task, which `trace::triage` also treats as carrying no failure signal) never counts as its recurrence. Harm = `error_clusters > 0 || takeovers > 0 || disabled`. Rule order: pinned ⇒ Continue; provisional: Retire, Keep, Dormant(12), Dormant(30 d since `status_since`, < 3 matched); kept: ≥ 2 harm events in the last 30 days and since it became kept ⇒ Retire. Count-time match uses `repo` (else `cwd`) against the glob, where a glob also matches descendants of the path it names. A `retrieve` hook is in a session's `loaded_entries` only through a `retrieval` record (§3), i.e. only in sessions where it injected units; `session_start.loaded_entries` never lists it.
Anchor replay (SPEC §Rollback and anchors; research D17, HCL b=1; `scope/anchor.rs`): a Keep of an entry that *replaces* a kept entry — its `sibling_of` names a kept entry, or keeping it takes the kept slot (`<skills>/<name>/`, agents `kept-agents/<name>/`) a kept entry holds, which `library::set_status` would supersede (rematch-owned entries only) — first replays the kept entry's originating task once. Anchor task, first with a verifier: the kept entry's `Origin::Task`, the replacing entry's, then a task in the kept entry's branch (any task for `shared`) whose last `runs/<task>/receipt.json` lists the kept entry (`staged[].id`, `staged[].merged_into`, `next_run[].id`, or the reference rollout's `loaded`); verifier = the task's own, else its last run's `job_spec.verifier`. The replay is one rollout (`batch::rollout::run_rollout`, label `anchor-<short id>`, the task's first seed, solver role) under the task's run lock: base library for the task repo minus both entries, the replacing entry's files as the only candidate. Pass ⇒ the keep proceeds and the replacing entry gets a receipt `Certificate{lint, canary: its counters, verifier: <verdict>}`. Fail ⇒ Retire with `status_note` `anchor replay failed: task <id> (<verifier>): <detail>`; the kept entry stays; the reflector input carries `anchor_replay`. No anchor task with a verifier ⇒ `NoVerifier`: the counters (the canary) are the anchor and the keep proceeds. A replay error (task locked by a running run, omp missing) fails the decision; the entry stays provisional and is decided again at its next matched session. Every replay record (task, verifier, verdict, rollout summary) goes to the blob store; the `count` envelope's decision carries `anchor: {kept, replay, certificate?}`. There is no anchor suite.

## 8. Rate limits

Per predicate_hash: ≤1 admission per UTC day; no reflection while a provisional entry with the same predicate_hash exists (pending canary). Per branch: ≤10 provisional; 5 kept general + 5 kept per branch before a new candidate must merge/revise (near-duplicate by simhash(normalized text) Hamming ≤ 6 or identical script body ⇒ append evidence to existing entry). ≤3 model calls per reflection (classifier batch, editor, one lint-retry).
Implementation (`rate::can_admit`, checks in this order): pending canary (any provisional entry with the predicate_hash) → `PendingCanary`; an admission today (UTC) → `DailyPredicate`; ≥ 10 provisional in the branch → `BranchProvisionalCap`; kept cap → `NeedsMerge` (for branch `shared`: ≥ 5 kept in `shared`; otherwise ≥ 5 kept in the branch). `NeedsMerge` is a routing signal: the caller merges/revises via `dedup::route_duplicate` instead of dropping the candidate. Built-in entries (§10) are neither canaries nor counted against the caps.

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
{"decision":"EDIT|KEEP_BASE","entry":{"kind":"skill|hook|script|fact|lesson|agent","name":"kebab","description":"...","trigger_line":"Use when ...",
 "files":{"SKILL.md":"...","units/x.md":"...","scripts/x.sh":"...","AGENTS.fragment.md":"..."},"hook":{...}|null,"stems":[...],"fork_path":"src/…"?},
 "manifest":{"evidence":{"session_id":"...","seq":0},"predicted_fix":"...","predicted_regression":"...","rationale":"..."}}
```
Classifier prompt: batched `[{id, text, after_deliverable, prelabel}]` → ```json `[{"id":..,"class":"correction|preference|scope|none","confidence":0-1}]```.

## 10. Extension (`extension/index.ts`, no build step)

Factory (async): read env/off; resolve binary (`$REMATCH_BIN` → `<ext>/../target/release/rematch` → `<ext>/../target/debug/rematch` → PATH); `pi.exec(bin, ["match","--cwd",cwd])` and `pi.exec(bin, ["skills","sync","--cwd",cwd,"--harness","omp"])` in parallel (5 s each); override `skills.customDirectories` with `[...configured, ...dirs]` (`dirs` of the sync = the shared `REMATCH_ROOT/skills`, §16 "Shared skills"); remember `entries`/`hooks` of the match (a failed sync leaves the hooks adopted, a failed match leaves the skills added); register tool `rematch {operation, arguments[]}` (approval: read for doctor/status/library/digest/match/why, `builtins ls`, `transfer suggest`, exec otherwise), command `/rematch`, shortcut `alt+e` (digest), handlers §3. `session_start`: open trace, write `session_start` (`staging_dirs` = the skill dirs added), spawn `reflect --pending` detached (`Bun.spawn` with `stdio ignore`, `unref`), run `digest` and present lines via `ctx.ui.notify`, and each approval via `ctx.ui.confirm(title, diff)` → `library approve|decline <id>`. `/rematch <sub>`: `why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|ship|transfer|builtins|anchor` map 1:1 onto the binary; long ones return a job id. **Re-match.** The loaded set (entries, hook modules, added skill dirs) is matched and synced again — and the hook sets swapped atomically, serialized with any in-flight match — when a main session's `ctx.cwd` differs from the cwd last matched (at `session_start` and on the next `context` event), on `session_switch`/`session_branch`, and after a successful `library approve|disable|rollback` or `activate` (digest approvals included), so an approved hook runs from the next tool call or request, not the next omp start; skill links a re-sync adds reach omp's skill list at its next start (discovery ran once, after the factory). The module-level `match` + `skills sync` cache (shared by subagent rebinds, keyed by binary + cwd) is cleared on a switch/branch and after those operations, so no cached result outlives the session it was matched for; a failed match or sync is not cached and leaves the previous set in place. A session's `session_start.loaded_entries` is what was loaded when it started: an entry approved mid-session runs at once and counts from the next session. Hook modules are imported as `<path>?v=<mtimeMs>` so a rewritten module is a new module record (Bun ignores the query of a `file:` URL for its module cache, so the path is used as is).

**Hook modes.** A compiled hook module (`library::hook_module`) is data only: `export default { id, mode, tool, field, regex, message, pass_condition, params }`, `params` = the resolved mode parameters (`HookRule::params`; for `block` the JS regex `{source, flags}`, a leading Rust `(?i)` becoming the `i` flag). The extension validates the shape (`hookFromModule`; an invalid module is skipped with a warning) and implements the modes; `tool: "*"` matches every tool.

| mode | event | behavior | on failure |
| --- | --- | --- | --- |
| `block` | `tool_call` | `input[field]` (a string) matches the regex ⇒ block with `message`, trace `tool_blocked` | closed: a throwing check blocks with `rematch hook <id> failed: …` |
| `stuck` | `tool_call` | the extension keeps the session's last 32 calls as `tool\0args_hash` (sha256 of canonical JSON, as in the trace); a call whose `repeats` immediately preceding calls are identical to it is blocked with `message` (trace `tool_blocked`). Any different call in between resets the run, so re-running a command after an edit passes; a call a `block` rule rejected never ran and is not recorded (one `stuck` rejected is, so the next identical call is rejected too). Reset on every session start | closed |
| `truncate` | `context` | walking back from the newest message, age = non-synthetic `user` messages after a `toolResult`; when age > `max_age_turns`, the result's text blocks become one block with the first `keep_chars` UTF-16 code units (one fewer when the cut would split a surrogate pair) + `\n…[rematch: truncated N chars, age K turns]` (N = code units removed); image blocks stay; several rules ⇒ the smallest `keep_chars`. A fresh object replaces the message in the outgoing array only | open |
| `retrieve` | `context` | main sessions with a trace only. Query = the last non-synthetic user message text + the paths (`arguments.path`/`file_path`, selectors stripped) of the toolCalls behind the last 10 `toolResult`s, ≤ 16 KB. `pi.exec(bin, ["retrieve","--cwd",ctx.cwd,"--budget",B,"--min-overlap",M,"--query",q], {timeout: 3000})` — no `--entries`: the binary already bounds candidates to the entries loaded for `--cwd`; the result is cached per argv. When units come back, ONE message is appended: `{role:"custom", customType:"com.vncp.rematch.retrieval", display:false, timestamp, content, details:{units:[{entry_id,path}]}}` with content `[rematch] Units of skills loaded in this session that match the current request, retrieved by the harness (the whole skill: skill://<name>).` followed by one `<unit skill="<name>" path="<path>">…</unit>` per unit; omp sends it as a `developer` message (§0). The same message object (same `timestamp`) is re-sent while the unit contents are unchanged. A changed unit set writes one `retrieval` trace record, the only way the hook enters `loaded_entries` (§7). Several retrieve hooks ⇒ the first one's parameters, every one's id in `entry_ids` | open: a failed or slow call adds nothing |

`truncate` and `stuck` also run in subagent sessions (like `block`); `retrieve` needs the main trace to attribute what it added. Handlers never throw; the `context` handler returns `undefined` when it changed nothing.

**Built-in entries** (`crates/rematch/src/builtins.rs`; SPEC §Isolation, research D22/D23). The context policies are shipped as three ordinary hook entries, not as always-on extension behavior: branch `shared`, repo glob `**` with no exts/stems (they match every session), `Origin::Import{source:"builtin"}`, family `unknown/unknown`, each with a SKILL.md router (trigger line, laws, pitfalls) and `hook.json`:

```json
{"mode":"retrieve","tool":"*","message":"rematch: units of the loaded skill packages that match this request.","pass_condition":"A request that shares no stem with a loaded package's trigger line, or whose loaded packages hold no unit sharing 2 distinct stems with it, passes unchanged; units only ever come from entries this session loaded, and at most 6000 bytes are added.","budget_bytes":6000,"min_overlap":2}
{"mode":"truncate","tool":"*","message":"rematch: old tool outputs trimmed in the outgoing request.","pass_condition":"Tool results at most 6 user turns old, and any result of at most 1200 characters, reach the model unchanged; the session file always keeps the full output.","max_age_turns":6,"keep_chars":1200}
{"mode":"stuck","tool":"*","message":"rematch stuck-detector: this exact call (same tool, same arguments) already ran 3 times in a row. Change the approach: different arguments, a different tool, or ask the user.","pass_condition":"Every call that differs from the call immediately before it passes, and so do the first 3 repetitions of any call; re-running a command after any other call (an edit, a read) passes.","repeats":3}
```
(`context-unit-retrieval` [D22], `tool-output-truncation` [D23], `stuck-detector` [D23]). `builtins::ensure` runs from `AppCtx::open_with` after the improver install: one read of `REMATCH_ROOT/builtins.version` (hash of the shipped specs) when nothing changed; otherwise, under `.builtins.lock`, it stages every built-in whose name has no built-in entry yet and rewrites the marker. A failure is logged to stderr and never fails the command. A retired, declined or dormant built-in is never re-staged automatically; `rematch builtins reinstall [name…]` stages a fresh provisional version (version + 1, `predecessor`) of each built-in that is not live, and `builtins ls` lists them with status, approval and counters. They arrive provisional and **unapproved**: the digest offers each as a hook approval (the module text as diff, reason = trigger line + pass condition). An unapproved hook entry is inert in full (`library::hook_active`): it contributes no branch dir, `match` reports `predicate_hit: false` (so it never enters `loaded_entries`), and no module exists. Approved, a built-in counts like any entry. It carries `redirect_class: None` on purpose: it was motivated by no particular redirect, so every correction or preference redirect in a session where it ran counts as recurrence, and error clusters, takeovers or a disable count as harm; two of either retire it (§7). A scope redirect does not count: the user widening or changing the task says nothing about a context policy. That makes a context policy prove itself against everything the user corrects while it is active. `context-unit-retrieval` is credited only where it acted: the extension leaves retrieve-mode hooks out of `session_start.loaded_entries`, and the hook enters a session's `loaded_entries` through the `retrieval` record (§3) written when it injected units. Built-ins take no admission-cap slot and are no pending canary (`rate::can_admit` skips `library::is_builtin`), and `ship --scope team` never selects them automatically.

## 11. Deviations from SPEC (all forced by §0)

1. `resources_discover` → factory-time `skills.customDirectories` override (same semantics, per process).
2. Batch isolation uses a `--config` overlay + fresh worktree instead of a private `PI_CODING_AGENT_DIR` (auth lives in the agent dir).
3. `omp plugin install @<org>/rematch` → `scripts/install.sh` (`cargo build --release`, `omp plugin link extension/`, PATH shim). No registry publish.
4. "Related thing: embeddings, exploratory only" is not implemented: the binary holds no provider credentials; exact signature + MinHash are implemented.
5. Status `pinned` is carried as the `Entry.pinned` flag (status and location unchanged; automatic transitions refuse pinned entries; `disable`/`rollback` are user actions and proceed). `Status::Pinned` remains a valid status for imports.
6. A kept entry replaced by a sibling becomes `dormant` (files in `predecessors/<name>/<version>/`) rather than `retired`, because `rollback` must be able to restore it and `retired` never leaves. A non-rematch skill that a kept entry replaced is saved as version `0` and restored by `rollback`/`uninstall`.
7. Declining an approval (`library decline`) retires the entry (SPEC: retired ← "approval declined").
8. Provisional agents cannot load "on match": omp discovers task agents only from fixed dirs (§0), so they load through the plugin root in every session and are counted where their glob hits. `off` does not unload them (omp reads the plugin's `agents/` itself); `library disable` or `omp plugin disable` does.

## 12. lint.toml schema

`improver/lint.toml` (copied to `REMATCH_ROOT/improver/` on first run). Every key is optional; a missing key takes the built-in value (`rematch_core::lint::DEFAULT_LINT_TOML`, which the shipped file equals verbatim — a test enforces it). All `[patterns]` entries are Rust `regex` syntax; use TOML multi-line literal strings (`'''…'''`) for patterns containing `'` (inside `'…'`, `''` is **not** an escape).

```toml
[limits]
always_on_bytes = 8192   # per branch, this entry + LintContext.branch_other_bytes: SKILL.md bodies (frontmatter excluded), AGENT.md `description` values, AGENTS.fragment.md files
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

Rules (`LintFinding.rule`): `identifiers`, `string_match_user`, `executable_risk` (applied to every file in the entry dir except `rematch.json` and `EXPERIENCE.md`; markdown is linted with its `rematch:` frontmatter block removed; `hook.json` contributes only `message` + `pass_condition`); `trigger` (non-empty repo glob, stem/ext caps, non-empty trigger line present in the SKILL.md body or any `Use when…` line, non-empty `description`; for `kind=agent` the document is `AGENT.md`: frontmatter `name` equal to the entry name and not `main`/`sub`, non-empty `description`, no trigger-line or form-order check on the system prompt); `scripts` (every `scripts/<path>` referenced in markdown exists; no script executable unless listed in `Entry.approved_scripts`); `always_on`; `form_order` (trigger line, then classified headings, non-decreasing in `form.order`); `facts_as_of` (`facts/*.md`, and SKILL.md of `kind=fact`, need frontmatter `as_of`); `hook` (`lint::hook_problems`, per mode: every mode a rule, a non-empty pass condition and a tool (`*` = any), no parameter of another mode; `block`: field, message, regex compiles and does not match everything; `stuck`: message, `repeats` in 2..=20; `truncate`: `max_age_turns` 1..=100, `keep_chars` 200..=100000; `retrieve`: `budget_bytes` 500..=65536, `min_overlap` 1..=20; non-block modes take no regex/field; `hook_module` refuses the same problems); `fragment` (`AGENTS.fragment.md` non-empty, no `#`/`##` heading outside a code fence: it is appended under `## rematch: <name>`); `fork_path` (`Entry.fork_path` repo-relative without `..`, `library::validate_fork_path`); `transfer` (`LintContext.transfer` and a domain lesson or fact). `LintReport.ok` iff no Error.

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
skills_cmd.rs  skills sync: the shared skills dir for a cwd (§16 "Shared skills"), Claude Code plugin link, Codex reach check, migration from claude-skills/ codex-skills/
reflect.rs     count + reflect --pending (§loop 1–8), editor/classifier calls, lint retry, stage, EXPERIENCE.md on decisions
create.rs      greenfield (sessions | task | description→draft), activate
improve.rs     brownfield (register, audit, split, unit-level propose, stage beside)
import_cmd.rs  import --history / --forge
digest.rs      session-start digest payload; capability-floor review list; transfer suggestions
retrieve.rs    retrieve: unit ranking for the `retrieve` context hook
builtins.rs    built-in context entries (ensure on open, `builtins ls|reinstall`)
doctor.rs      doctor
uninstall.rs   uninstall
batch/         Phase 2: tasks.rs (task defs), substrate.rs (worktree + overlay materialization), rollout.rs, verifier.rs, run.rs (k=1/width scheduler), worker.rs (run-worker)
scope/         Phase 3: ship.rs (team/fork PR), export.rs (work↔home export/import), stale.rs (drift), split.rs (per-family split), transfer.rs; anchor.rs (anchor replay before a replacement of a kept entry, §7)
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
- `import --forge`: skills register in branch `forge` with `path` = the forge skill dir; each `learnings/*.md` becomes a provisional `lesson` at `REMATCH_ROOT/lessons/forge/lesson-<slug>/` (`LESSON.md` copy + `rematch.json` with `evidence_sha256` = blob hash of the file, `source_path`). `match` omits lessons. `import --history` keeps an existing `source: trace` session row (richer) instead of overwriting it, and records the session file as `source_path`; `count` folding a synthesized trace refreshes the import row's signals and keeps it `source: import` (counted once, never replaced by a trace row).
- `improve`: the audit counts every ledger session with the skill in `skill_reads` (traced and imported); < 5 ⇒ `{refused, traced_sessions, offer}`. The excerpt and clusters come from each signalling session's live trace, else the trace synthesized from its omp session file (`import_cmd::records_for_session`: recorded `source_path`, else `<agent dir>/sessions/<cwd slug>/*_<id>.jsonl`, else a scan); the moment is the most recent session's earliest undismissed redirect, else the first cluster of the session with the most error clusters; `clusters` lists redirect clusters and `error_cluster` groups (failing tools, error heads). Siblings share the base predicate (no pending-canary check), get `sibling_of` and stage at `staging/<branch>/<name>/` where branch = `--branch`, else the reading sessions' common repo, else `shared`.
- `create --from sessions`: request stems (name + description) and `--files` extensions vs. session stems/exts; a session matches with overlap ≥ min(2, |request stems|); ≥ 3 needed; up to 8 references. `activate --task` runs one reference rollout and attaches its moment as evidence before draft → provisional.
- `digest` lines: one per entry whose status changed since `REMATCH_ROOT/digest.last` (imported registrations summarized in one line), plus Scope review/stale/transfer lines; approvals = unapproved scripts or uncompiled hooks of owned provisional/kept entries (hook reason = trigger line or manifest rationale + `Pass condition: …`), plus `export:<id>` items.
- `reflect` content-triage failures are logged (stderr + envelope) and the pending file deleted; no ledger proposal row.

## 15. Scope (Phase 3, `crates/rematch/src/scope/`)

CLI additions: `stale [--dry-run] [--recanary <entry>]`, `split [--review-days 7]`, `transfer promote`; `ship` adds `--push`, `--pr` (needs `--push` and `gh`), `--remote origin`; `library export` adds `--entry <id>… --approve | --approve-all`.
New `Entry` fields (serde default, omitted when empty): `fork_path`, `fork_repo`, `fork_commit`, `families_excluded`, `links` (`transfers_from`).

| Command / fn | Behavior |
| --- | --- |
| `ship --scope team` | Kept/provisional entries whose predicate content (`count_hit`, ignoring the one-repo glob) hit sessions in ≥ 2 distinct repos, or listed with `--entry`. Target = `--repo` or the originating session's repo. Lint must pass. A temporary worktree from HEAD gets branch `rematch/<name>-<last 8 id chars>` with `.omp/skills/<name>/` (no `rematch.json`/`EXPERIENCE.md`/`AGENTS.fragment.md`, mode 0644, `rematch.scope: team`) — an agent ships as the single file `.omp/agents/<name>.md` (its AGENT.md) — and, when the entry has `AGENTS.fragment.md`, `<repo>/AGENTS.md` with the section `## rematch: <name>` (file created if missing; an existing section of that heading, up to the next `#`/`##` heading, is replaced: `ship::upsert_agents_section`), committed with an evidence body (counters, per family, matched sessions, repos, families, redirect class, predicted fix/regression, latest EXPERIENCE.md record). Receipt `activation: pr`. No network unless `--push`/`--pr`; never merges. |
| `ship --scope fork` | Entries with `fork_path` (set by `library set-fork-path <id> <path> [--repo <fork>]` — ledger + `rematch.json`, a changed target clears `fork_commit` — or proposed by the editor as `entry.fork_path`; a hand edit of `rematch.json` is adopted too). Target = `--repo`, else the entry's `fork_repo`. Content = `fork/<basename>` in the package, else the SKILL.md (agents: AGENT.md) body. Body adds `omp --version`, file sha256, base commit, before numbers (the predicate over sessions that started before the entry existed, `recurred`/`harmed` rules, per family) and after numbers (entry counters). Records `fork_repo` + `fork_commit` (base HEAD). |
| `library export` | `policy.json` `{environment: work|home, export_enabled}`; missing ⇒ home; work refuses unless `export_enabled`. Candidates: kept entries with `lesson_tag: invariant` or `manifest.scope: invariant`. Each is linted with `transfer: true`, `facts/` dropped, `rematch.json` stripped (provisional, no counters/evidence/approvals/path). Without approval the result lists `pending` with `git diff --no-index` diffs and `requires_approval: true`; `--entry <id> --approve` or `--approve-all` writes `<export>/<id>/` and commits it (the export dir is `git init`ed on first use). Receipt `activation: prompted`. `export::decline` records `{id: content sha}` in `export-declined.json` so the digest stops offering it until the content changes. |
| `library import-export` | Each `<dir>/<id>/rematch.json`: skip when already present (same id or `transfers_from` link), not invariant, or a near-duplicate (`dedup::route_duplicate`) of a live entry; branch = recorded branch when this library knows it, else `shared`; repo glob = that branch's latest session repo, else `**`; lint `transfer: true`; staged provisional with origin `Import{source: "export:<id>"}`. |
| `stale` | Kept/provisional, unpinned entries go `stale` when (a) a `facts/*.md` `source:` file's sha256 differs from its `source_sha256` frontmatter (written by `stale::snapshot_sources` after staging) or the file is gone, (b) the last 3 counted sessions in the branch (all branches for `shared`) all ran a family ≠ `family` and ∉ `verified_on`, (c) `git rev-parse HEAD` of `fork_repo` ≠ `fork_commit`. `--recanary` stages a new version (new id, version + 1, `predecessor`, family = the one now in use, fork commit cleared, sources re-snapshotted) and retires the stale one. |
| `split` | `counters::family_divergence` ⇒ sibling `<name>--<family slug>` (provisional, same branch, `family` = harmed, `sibling_of`, version + 1, original files + `units/family-note.md`, linted); the original gets `families_excluded += harmed` (informational: entries load regardless of family). Idempotent. `capability_review(window)`: families whose recurrence or harm rose on ≥ 2 entries in the last window vs the one before; review list only. |
| `transfer suggest / apply / promote` | Facets: `branches/<b>/domain.yml` `facets:` else distinct outcome facet keys of the branch (`docs/domain.yml.md`). Suggest kept invariant lessons for branches overlapping on ≥ 2 keys and not yet tried (same name, link, near-duplicate). Apply: provisional copy (new id, `links: transfers_from`, origin `Import{source: "transfer:<id>"}`, repo glob of the target branch, `facts/` dropped, approvals reset), lint `transfer: true`, rate limits except `NeedsMerge`. Promote: an entry kept in one branch plus kept (same name, link or near-duplicate) or a clean canary (matched ≥ 4, no recurrence/harm) in another ⇒ copy to `shared` (repo glob `**`) and keep it at once; `library` moves the branch version to predecessors (dormant). When the copy takes a kept entry's slot, the anchor replay (§7) gates the keep: pass ⇒ kept plus an anchor receipt (`canary: null`); fail ⇒ the copy is retired with `anchor replay failed: …`, the kept entry stays, and that entry is not promoted again (a retired copy linking it with that note settles it); a replay error ⇒ the copy is retired with `anchor replay deferred: …` and the next pass retries; no verifier ⇒ kept. The promotion row carries `anchor: {kept, replay | error, certificate?, retired?}`. |

File ownership (all scope status changes and rematch.json writes): an entry's files are rematch's only when it is not `reflect::is_foreign` and its dir holds a `rematch.json` with its own id (`scope::owns_files`). User-owned entries (brownfield registrations in place, forge imports) get status changes in the ledger only (`reflect::record_status`); their dirs are never moved or written, and they are never `shared`-promotion sources. Entries with family `unknown/unknown` (imports) never go stale on the model-family rule.

## 16. Harness surfaces and how each is reached

Harness² edits eight surfaces (research D3). Each maps to one omp component; activation follows SPEC §"Scope and who merges" (personal text automatic as `provisional`; executable or blocking behind a one-tap approval; team and fork only as a branch/PR the binary never merges).

| Surface | omp component | Mechanism in rematch | Activation | Status |
| --- | --- | --- | --- | --- |
| System prompt | `<repo>/AGENTS.md` (context file); per-model prompt text in the omp fork | `AGENTS.fragment.md` in an entry → section `## rematch: <name>` of `<repo>/AGENTS.md` (`ship --scope team`); fork prompt files via `fork_path` (`ship --scope fork`) | team / fork PR only | built; never written locally (always-on for everyone in the repo, so it needs review) |
| Guardrails / rules | laws in a triggered skill's router `SKILL.md` | `## Laws` of a skill entry (`kind: rule` entries are skills); lint form order | automatic as `provisional`, counters keep or retire | built; standalone always-on rule files deferred (they would cost every session) |
| Memory / facts | `facts/*.md` inside a skill (each with `as_of`), `EXPERIENCE.md` beside entries | fact entries and units; `stale` on source drift; the reflector writes EXPERIENCE.md after keep/retire and the editor reads matching records | automatic; domain facts never transfer | built; omp's own memory backend is not written |
| Tool guidance | the fork's tool descriptions / rules text | `fork_path` (`library set-fork-path` or `entry.fork_path` from the editor) + `fork/<basename>` → `ship --scope fork` | fork PR only | built |
| Subagents | `~/.omp/agent/agents/<name>.md` (user), `<extension-root>/agents/*.md` | `kind: agent` entry (`AGENT.md` + `rematch.json`); provisional: `extension/agents → REMATCH_ROOT/agents/<name>.md → staging/<branch>/<name>/AGENT.md`; kept: copied to `~/.omp/agent/agents/<name>.md` (predecessor saved); team: `.omp/agents/<name>.md` | automatic as `provisional` (visible in every session, counted where the glob hits) | built |
| Skills | `~/.omp/agent/skills/<name>/SKILL.md` | linked into the shared dir `REMATCH_ROOT/skills/` (below), which the factory adds to `skills.customDirectories`; kept: copied natively to `~/.omp/agent/skills/<name>/` and linked there too (one skill by realpath, §0); team: `.omp/skills/<name>/` | automatic as `provisional` | built |
| Scripts | `scripts/<n>` inside a skill | staged mode 0644; `library approve` (or the digest's one tap) makes them 0755 | one tap | built |
| Hooks / plugins | `~/.omp/agent/extensions/rematch-hooks/<id>.ts` (data-only modules), imported by the extension and run on `tool_call` (`block`, `stuck`) or `context` (`truncate`, `retrieve`) | `kind: hook` entry (`hook.json` with a `mode`), compiled on approval; the three built-in entries (§10) are hook entries of the context modes | one tap | built: block verifiers, stuck detection, age-based tool-output truncation, harness-driven unit retrieval; other plugin shapes (error hints) are not generated |

### Shared skills (`REMATCH_ROOT/skills/`)

One directory delivers skill entries to every harness: `REMATCH_ROOT/skills/<name>` → the entry dir (`staging/<branch>/<name>/` or the kept `~/.omp/agent/skills/<name>/`). `library::sync_shared_skills` writes it; `rematch skills sync --cwd <dir> [--harness omp|claude-code|codex]` runs it for a cwd (its repo, else itself) and returns `{added, pruned, total, dirs, links, untouched, off, …}`.

| Harness | Reads the shared dir through | Synced for a cwd by |
| --- | --- | --- |
| omp | `skills.customDirectories += [REMATCH_ROOT/skills]`, set in the factory (per process, §10) | the factory (`skills sync --harness omp`) |
| Claude Code | the plugin's `skills/ → REMATCH_ROOT/skills` (created or re-pointed at `SessionStart`) | the `SessionStart` hook (`reloadSkills: true` when a link changed, §18.1) |
| Codex | `~/.agents/skills/rematch → REMATCH_ROOT/skills` (one link the user makes; Codex scans user roots recursively, §18.2.1) | `skills sync --harness codex --cwd <repo>` before Codex starts (§18.2.3) |

- **What is linked.** Every kept or pinned entry (status `kept`/`pinned` or the human pin), and every provisional entry whose predicate matched a cwd that was synced — the load-time rule of `list_for_match` (repo glob against the repo, else the cwd; hooks only once approved). Only entries rematch owns (their dir holds their own `rematch.json`; a user skill registered in place is never linked) with a `SKILL.md`; agents go through `agents/` (Subagents row), lessons are library records. A link targets the entry's recorded path, so a skill kept under one omp profile's agent dir loads in every profile (`REMATCH_ROOT` is shared, §1).
- **Loading is the union of the branches used on this machine.** A sync adds the cwd's matches and never removes a provisional link because this cwd does not match it: another session, in another repo or another harness, may be loading it.
- **Pruning.** A link goes when its entry stops being provisional/kept/pinned (retired, dormant, stale, back to draft, deleted) or moved (a keep re-points it to the kept dir), and every link goes while rematch is off. Every status change runs the sync without a cwd — `set_status` (keep, retire, dormant, stale), `stage`, `activate`, `rollback`, pin/unpin, `off`/`on` — so a retirement leaves the dir at once and a keep re-points at once; only a cwd sync adds provisional links.
- **Counting is unchanged.** Each session's `loaded_entries` still comes from `list_for_match` for its own cwd (Codex: the import's count-time predicate, §18.2.3): an entry loaded through the shared dir whose predicate misses the session is not counted. The union therefore costs one description line per extra skill in the system prompt and never a counter.
- **One name, one link.** A provisional entry matched by the current sync takes its name; a provisional candidate already linked shadows the kept entry of the same name until it is kept or retired; kept/pinned entries fill every other name. Non-symlinks in the dir are never touched (`untouched`); hidden names are skipped.
- **Atomic and idempotent.** Each link is written as a hidden temporary symlink renamed over the name, so a reader sees the old target or the new one, never a missing name; a sync with nothing to change writes nothing (`added` and `pruned` empty).
- **Race rule.** Syncs run without a lock: each only adds links to live entries and removes links to dead ones, so concurrent syncs converge; two matching cwds wanting different entries under one name leave the last writer's link; a prune re-reads the link and removes it only while it still points where the sync saw a dead entry. A session sees the dir as of its own skill scan (omp at process start, Claude Code at `SessionStart` or its re-scan, Codex at session start); a link pruned under a running session dangles there — retirement wins over a running session, and a link added later reaches the session's successors.
- **Migration.** The first sync removes the per-harness link dirs of older versions, `REMATCH_ROOT/claude-skills/` and `REMATCH_ROOT/codex-skills/` (their links; a dir holding anything else stays). `--harness codex` also re-points `~/.agents/skills/rematch` and `<repo>/.agents/skills/rematch` when they still point at `codex-skills`, and removes the kept copies older versions wrote under `~/.agents/skills/<name>/` (marker `.rematch-codex.json`); `uninstall` removes the shared dir (links only, never their targets) and any such copies.

## 17. Batch mode (Phase 2, `crates/rematch/src/batch/`)

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

Worker (`run-worker`): reference rollout (pending entries of the task loaded) → next-run rule: entries with `pending_next_run` and `Origin::Task{task}` go `kept` (+ `verified_on`, receipt) on a reference pass, `retired` on a fail; a pass for an entry that replaces a kept entry first runs its anchor replay (§7; the run's omp binary, this task's lock reused when the anchor task is this task): replay fail ⇒ `retired` with the `anchor replay failed: …` note and the kept entry stays, replay pass ⇒ `kept` plus a second receipt with the anchor verdict, replay error ⇒ the flag stays for the next run; the row in `next_run` carries `anchor: {kept, replay, certificate?}` → if the reference passes a score-less verifier the cycle stops (nothing can rank above it) → k invariant + k domain editor calls (`editor.md`; input = §9 shape with `redirect {class: correction, text: verifier detail}`, `task.prompt`, `verifier.verdict`, `prior_candidates` for i > 1; excerpt = around the longest consecutive tool-error stretch, else the trajectory tail), each linted and probed once → every (invariant, domain) pair composed: the best pair by one editor call with `scope: compose` (+ `candidates`, `behavioral_diff`) while the model-call budget lasts, the rest (and composer KEEP_BASE/failure/lint failure) by deterministic merge (union of files; domain laws that are simhash near-duplicates of an invariant law dropped; same-named entries merged) → each composed harness runs once; with ≥ 2 composed variants the best runs once more and must pass both → selection: (pass, score) desc, then tool errors, turns, lesson count, order; a winner must pass and beat the reference, else KEEP_BASE → winner lessons: a near-duplicate of a branch entry appends evidence to it; else one `rate::can_admit` per winner, staged provisional (origin Task, branch/predicate of the task repo, `lesson_tag` by scope, `pending_next_run: true`, receipt with the verdict). Budget at width k: rollouts = (k+1)² + [k>1] (4 at k=1, 10 at k=2), model calls = 2k + 1; anchor replays are outside it (one per replacing pending entry) and are not in the receipt's `rollouts`. Progress in `jobs/<id>.json` (`phase`, `rollouts_done`, `rollouts_budget`, `model_calls`, `verdicts`); receipt `runs/<task>/receipt.json` (+ blob) with every rollout summary, proposal, lint report and the decision; workspaces and worktrees are removed after the receipt (also on failure). One run per task at a time (`runs/<task>/.lock`).


## 18. Adapters

One binary, one ledger, one lint and one set of counters and entries serve every harness. An adapter only forwards what its harness exposes; the trace schema (§3), the flag rule, matching and hook evaluation stay in Rust. Session ids are prefixed per harness so the ledger and `traces/` never collide: omp ids are bare UUIDs, Claude Code sessions are `cc:<uuid>` (`cc:<uuid>:agent-<id>` for an imported subagent transcript), Codex sessions `codex:<id>`. `import_cmd::Transcripts` dispatches on the prefix wherever a ledger session's transcript is read back (`records_for_session`, `reflect --history`, `improve`).

### 18.1 Claude Code (`adapters/claude-code/`)

**Verified facts** (Claude Code 2.1.285: `claude --help`, `claude plugin --help`, `claude plugin validate`, `claude --plugin-dir <dir> plugin details <name>`; the hooks, skills and plugin references at code.claude.com/docs; the CHANGELOG versions in brackets):

- A plugin is a directory with `.claude-plugin/plugin.json`; `hooks/hooks.json`, `commands/*.md` and `skills/<name>/SKILL.md` are its default component locations. One session: `claude --plugin-dir <dir>`. Every session: a plugin directory under `~/.claude/skills/<name>/` loads as `<name>@skills-dir` (`claude plugin init` help text). `~/.claude/plugins/` is the marketplace cache (`installed_plugins.json`, `cache/`), and settings know only `enabledPlugins` keyed `name@marketplace`; a marketplace install copies the plugin and drops symlinks that leave it, so it is not a route for this adapter.
- Command hooks in exec form: `command` + `args` [2.1.139], `${CLAUDE_PLUGIN_ROOT}` substituted and exported. Payload on stdin; common fields `session_id`, `transcript_path`, `cwd` (follows `cd`/worktrees), `hook_event_name`, `permission_mode`, plus `agent_id`/`agent_type` when a subagent's tool fires the hook. Event fields: `SessionStart` `source` (`startup|resume|clear|compact|fork`), `model` (optional); `UserPromptSubmit` `prompt` (as typed; also fired for background-task reports and cross-session messages); `PreToolUse` `tool_name`, `tool_input`, `tool_use_id`; `PostToolUse` + `tool_response` (successful calls only); `PostToolUseFailure` + `error`, `is_interrupt` [2.1.119]; `Stop` `last_assistant_message` [2.1.47] (not fired on a user interrupt); `StopFailure` (API error) [2.1.78]; `SessionEnd` `reason`.
- Exit 2 on `PreToolUse` blocks the call and Claude reads stderr as the reason; any other non-zero exit is a non-blocking error; exit 0 with one JSON object on stdout is structured output. `UserPromptSubmit` `hookSpecificOutput.additionalContext` adds context next to the prompt; above 10,000 chars Claude Code moves it to a file. `SessionStart` `hookSpecificOutput.reloadSkills: true` re-scans skill directories after the SessionStart hooks [2.1.152] (discovery normally runs before they finish). `SessionEnd` hooks get 1.5 s; a plugin's `timeout` does not raise that budget (`CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS` does).
- A plugin's `skills/` that is a symlink to a directory elsewhere, and symlinked skill folders inside it, are followed by a session: `claude plugin validate` says so ("A session loading this plugin does follow it") and `claude --plugin-dir … plugin details` listed the skills behind both kinds of link. Plugin skills are namespaced `/<plugin>:<skill>` (the `Skill` tool's `skill` input is `rematch:<name>`).
- A plugin's `agents/*.md` load from a real file: probed with `claude --plugin-dir <probe> plugin details` (2.1.285, `CLAUDE_CONFIG_DIR` pointed at a temp dir), an agent file inside a symlinked `agents/` directory is listed, a symlinked agent file (in a real or a symlinked `agents/`) is not. `REMATCH_ROOT/agents/` holds file symlinks (`<name>.md → staging/…/AGENT.md`), so the plugin does not link it: provisional agents stay omp-only (limitation 8).
- Commands substitute `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_SESSION_ID}` [2.1.9] and `$ARGUMENTS`, and run ``!`cmd` `` lines before the prompt reaches the model when `allowed-tools` pre-approves them.
- Transcripts: `~/.claude/projects/<slug>/<uuid>.jsonl`, subagents under `<uuid>/subagents/agent-<id>.jsonl` (`$CLAUDE_CONFIG_DIR` relocates `~/.claude`). Format facts relied on: module doc of `rematch_core::import_claude`.

**Plugin.** `hooks/hooks.json` wires `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure`, `SessionEnd` to `sh scripts/hook.sh trace-event` and `PreToolUse` to `sh scripts/hook.sh hook-check`. `hook.sh` pipes the payload unchanged to `rematch <op> --harness claude-code --hook-output` (binary: `$REMATCH_BIN` → `<repo>/target/{release,debug}/rematch` → `PATH`, as the omp extension) and never fails the hook: it exits 2 only for an intentional block (binary exit 2 with nothing on stdout), prints stdout only on exit 0, and exits 0 otherwise. Under `--hook-output` the binary prints the hook reply (`data.hook`: stdout JSON, stderr, exit) instead of the envelope, and every error, including a usage error, exits 0. `commands/rematch-{status,why,mark}.md` run `rematch --text status` + `--text skills sync --harness claude-code`, `library why [$ARGUMENTS]`, and `mark --session cc:${CLAUDE_SESSION_ID} --class $ARGUMENTS` through ``!`…` `` lines; they hold no prompt text beyond "show the output".

**Events → trace** (`crates/rematch/src/trace_event.rs`; one process per hook, so the state the omp extension keeps in memory is read back from the trace, which is `flock`ed while a record is appended):

| Hook | Records | Notes |
| --- | --- | --- |
| `SessionStart` | `session_start` (`mode: "claude-code:<source>"`, `omp_version: "claude-code"`, model from the payload, `staging_dirs` = `[REMATCH_ROOT/skills]`) | matches the payload `cwd` (`library::list_for_match`), syncs the shared skills dir for it (`skills_cmd::sync_cwd`, §16 "Shared skills"), creates or re-points `<CLAUDE_PLUGIN_ROOT>/skills → REMATCH_ROOT/skills`; replies `reloadSkills: true` when a link changed. `source: compact` refreshes the links without a second `session_start`; `resume` appends one to the same trace (a session counted before it was resumed is not counted again). `loaded_entries` = predicate hits minus lessons, agents, `truncate` hooks and `retrieve` hooks (credited through `retrieval`); `hooks_loaded` = the approved `block`/`stuck`/`retrieve` modules |
| `UserPromptSubmit` | [`agent_end`] `input`, [`retrieval`] | injected frames (`is_injected_frame`) are skipped; `idle` = no `input` since the last `agent_end` (a prompt after Esc or queued while Claude worked → `interrupt`, and the open run is closed first); prelabel against the trace's previous `input`, `after_deliverable` = the last `turn_end.text_head`; `/rematch-mark <class>` (also `/rematch:rematch-mark`) is spelled `/rematch mark <class>` (`import_claude::canonical_prompt`); other commands except `/skill:` are dropped as in omp. An approved `retrieve` hook ranks the loaded packages' units for the prompt plus the paths of the last 10 tool results (`retrieve::rank`, the module's `budget_bytes`/`min_overlap`) and, when the unit set differs from the last `retrieval` record, writes one and replies `additionalContext` with the units |
| `PostToolUse` / `PostToolUseFailure` | `tool_result` | `import_claude::tool_result_record`: tool name lowercased (`Read` → `read`, `Bash` → `bash`, so omp-authored hook rules and file derivations apply), `is_error` = the failure event, `content_head` from `tool_response` (`stdout`/`stderr`, `file.content`, `content`, else the JSON) or `error`, `skill_read` from the `Skill` tool (`plugin:name` → `name`) or a `Read` of `…/skills/<name>/SKILL.md`, `path` from `file_path`/`path`/`notebook_path` |
| `Stop` / `StopFailure` | [`model_change`] `turn_end`…, `agent_end` | one `turn_end` per API response in the transcript tail (last 8 MB) later than the last `input`/`turn_end`/`agent_end` (model, usage, stop_reason, text); if the transcript lags, one `turn_end` with `last_assistant_message`; `yielded` = `Stop` |
| `SessionEnd` | [`agent_end`] `session_shutdown` | `trace::flag_reasons` (the extension's rule); flagged ⇒ `pending/cc:<uuid>.json` (`source: "claude-code"`) |
| `PreToolUse` | [`tool_blocked`] | `hook-check`: the approved `block`/`stuck` modules matched for `cwd`, read back from `hooks_dir/<id>.ts` (the approved data, `parse_hook_module`). `block`: the Rust `regex` on `tool_input[field]` (`path` and `file_path` stand in for each other); `stuck`: identical calls (tool + `args_hash`) right before this one, read from the trace (`tool_result`s and `stuck` rejections count, `block` rejections are skipped) ≥ `repeats`. A rejection writes `tool_blocked` (with `args_hash`) and exits 2 with the hook's message on stderr. Subagents (`agent_id`): `block` applies, `stuck` does not (no trace) |
| subagent events, other events | — | the adapter traces main sessions only, as the extension |

**Skill loading.** The plugin's `skills/` is a symlink to the shared dir `REMATCH_ROOT/skills/` (§16 "Shared skills"), created at the first SessionStart (the repo cannot ship an absolute link, so `adapters/claude-code/.gitignore` lists it) and re-pointed if an older version left it at `REMATCH_ROOT/claude-skills`, which that first sync deletes. `SessionStart` and `rematch skills sync --harness claude-code --cwd <dir>` (which also fixes the plugin link when `$CLAUDE_PLUGIN_ROOT` is set) run the sync; kept entries are linked there from `~/.omp/agent/skills/<name>/`, so nothing is copied into `~/.claude/skills/`: nothing outside `REMATCH_ROOT` and the plugin directory is written, and `rematch uninstall` (which removes `REMATCH_ROOT`) leaves only the dangling `skills` link in the plugin.

**History.** `rematch_core::import_claude` (`scan_sessions_dir`, `parse_session`, `trace_records_of`, `session_record`, `find_session_file`): typed prompts (`origin.kind` absent or `human`, not `isMeta`/compaction summaries/injected frames/local command output; `[Request interrupted by user…]` marks an aborted run, so the next prompt is `interrupt`; a queued prompt is `interrupt`; `<command-name>/x</command-name>…<command-args>a</command-args>` → `/x a`) → `input`; `tool_use` + `tool_result` → `tool_result`; entries sharing `message.id` → one `turn_end` (thinking blocks dropped, `<synthetic>` model ignored); `agent_end` before each prompt and at the end; `session_shutdown` by `flag_reasons`. Subagent transcripts have no `input`s (their user text is the parent's) and are never reflected. `import --history --harness claude-code|all` folds each transcript into a `source: "import"` row; outcomes are not extracted (they come from omp tool receipts).

**What Claude Code cannot do** (compared with the omp extension):

1. `truncate` hooks: no hook rewrites earlier tool results or the outgoing request. `PostToolUse` `updatedToolOutput` [2.1.121] replaces a result only as it arrives (age 0), while `truncate` shortens results once they are `max_age_turns` user turns old, so `truncate` entries are not run and not counted.
2. `retrieve` runs once per prompt, not before every model request, and the injected units stay in the conversation (omp re-sends one non-persisted message per request); a unit set equal to the last one is not injected again.
3. Skill loading is not per process: the shared dir serves every Claude Code session (and omp and Codex), holding the union of what every synced cwd matched, and a concurrent session sees links added after its scan only after its next re-scan. Its trace still lists, and counts, only what was matched for its own cwd.
4. No per-turn event: `turn_end` records are recovered at `Stop` from the transcript tail, which can lag; then one `turn_end` without model/usage carries `last_assistant_message`. Model fallbacks inside a run are seen only as turn-to-turn model changes.
5. Esc fires no hook: an interrupt is inferred at the next prompt (no `Stop` since the last `input`).
6. The `stuck` window sees completed calls only (parallel identical calls in flight are not counted); subagents get `block` rules but no `stuck` detection.
7. Hooks cannot open dialogs: no session-start digest or one-tap approval, and no `rematch` tool for the model. Approvals and digests are `rematch digest` / `rematch library approve <id>` in a shell; reflection is `rematch reflect --pending` (shell, cron, or the next omp session start) — no model call ever runs inside a Claude Code session.
8. Provisional `kind: agent` entries are omp agent files and are not loaded (excluded from `loaded_entries`): Claude Code does not read the file symlinks of `REMATCH_ROOT/agents/` (verified facts above).
9. A session killed before `SessionEnd` (or past its 1.5 s budget) keeps an unfinished trace that `count` skips — the same failure mode as an omp crash.
10. Imported Claude Code sessions carry no `loaded_entries` (transcripts do not record what the plugin linked).

### 18.2 Codex (`adapters/codex/`, `rematch_core::import_codex`)

Codex gets no runtime code from rematch: evidence comes from its session rollouts after the fact, entries reach it as skill directories. No hook, no live trace, no model call inside a Codex session.

#### 18.2.1 Verified Codex facts

Rollouts: 86 local files under `~/.codex/sessions/` written by Codex Desktop `cli_version` 0.163.0-alpha.10.2 (read-only inspection, 2026-10-26); skill discovery: [developers.openai.com/codex/skills](https://developers.openai.com/codex/skills) and `openai/codex` main (`codex-rs/ext/skills/src/loader/{discovery,host,mod}.rs`, `codex-rs/skills/src/{parser,mentions}.rs`); `exec --json`: [developers.openai.com/codex/noninteractive](https://developers.openai.com/codex/noninteractive) and `sdk/typescript/src/items.ts`.

| Fact | Consequence |
| --- | --- |
| Rollouts live at `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local ts>-<uuid>.jsonl` (`CODEX_HOME` defaults to `~/.codex`); each line is `{timestamp, ordinal, type, payload}` | `scan_sessions_dir` walks for `rollout-*.jsonl`; the id is `session_meta.payload.id` (else the file name's last 36 chars); ledger id `codex:<id>` |
| Line types seen (count over 86 files): `event_msg/item_completed` 8815, `response_item/{reasoning 3608, custom_tool_call 2188, custom_tool_call_output 2187, message 1662, function_call 209, function_call_output 209, compaction 15, additional_tools 20}`, `event_msg/token_count` 2826, `token_usage_record` 2762, `event_msg/thread_settings_applied` 426, `turn_context` 378, `event_msg/{task_started 355, task_complete 345, turn_aborted 8}`, `world_state` 131, `session_meta` 86, `compacted` 24 | unknown types are ignored |
| `session_meta.payload`: `id`, `timestamp`, `cwd`, `originator`, `cli_version`, `source` (`"exec"` 67, `"vscode"` 2, `{"subagent":{"other":"guardian"}}` 17), `thread_source`, `model_provider`, `parent_thread_id`, `base_instructions` | `is_subagent` = `source.subagent` or a `parent_thread_id`: reviewer threads, whose "user" turns are transcripts written by the parent, are imported but never reflected |
| Model per turn: `turn_context.model` (+ `effort`); `thread_settings_applied.thread_settings.{model, model_provider_id}` | `turn_end.model` = `{provider: model_provider, id}`; a change between turns → `model_change` |
| Two representations of the same conversation. Thread items (`event_msg/item_completed.item`): `UserMessage{content:[{type:text,text}\|{type:local_image,path}]}`, `AgentMessage{content:[{type:Text,text}], phase: commentary\|final_answer}`, `CommandExecution{command:["/bin/zsh","-lc",script], cwd:"file://…", parsed_cmd:[{type: read\|search\|list_files\|unknown, cmd, path}], status: completed\|failed, aggregated_output, exit_code}`, `FileChange{changes:{<path>:{type: add\|update\|delete, …}}, status}`, `McpToolCall{server, tool, arguments, status, result:{content, isError}}`, `ImageView{path}`, `Extension{kind:"web.search", query}`, `Reasoning`, `ContextCompaction`. Model-facing items (`response_item`): `message{role: developer\|user\|assistant}`, `function_call{name, namespace?, arguments: JSON string, call_id}` (`exec_command {"cmd"}`, `wait`, `write_stdin`, namespaced MCP), `function_call_output{call_id, output: string ("…Process exited with code N…") \| [{type:input_text,text}]}`, `custom_tool_call{name: exec (2187: a JS cell that calls tools, whose commands appear as thread items) \| apply_patch, input}`, `custom_tool_call_output` (`Script completed\|Script failed…`) | thread items when the file has any (one result per real command, clean user text), else response items |
| User text is wrapped: Desktop prefixes requests with `<in-app-browser-context source="ambient-ui-state">…</in-app-browser-context>` and `## My request:`; automations send `<heartbeat>…</heartbeat>` turns (`turn_trigger: automation_heartbeat_scheduled`); answers to the agent's `request_user_input` arrive as `<send_user_message_question_reply>[{question, answer}]</…>`; response-item user messages also carry `<environment_context>`, `<user_instructions>`, the AGENTS.md preamble | only the human's words become `input`; heartbeats and frames are dropped; a question answer is an idle input with `source: "question_reply"` |
| Turns: `task_started{turn_attribution.turn_trigger: composer\|exec\|automation_heartbeat_scheduled\|resume_interrupted_task\|guardian_review}`, `task_complete{last_agent_message}`, `turn_aborted{reason:"interrupted"}`; a `UserMessage` with the turn id of a running turn is a mid-turn steer (34 turns locally) | `agent_end` at `task_complete` (yielded) / `turn_aborted`; the next input after an abort, and a steer, are `interrupt`s (the extension's `!ctx.isIdle()`) |
| Usage: `token_usage_record.usage` (one per response) and `event_msg/token_count.info.last_token_usage`, both `{input_tokens (includes cached), cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}` | `Usage{input: input − cached, cacheRead: cached, cacheWrite, output, totalTokens}`; `token_count` only when the file has no `token_usage_record` (they duplicate) |
| The skill list given to the model is in the rollout: `world_state.state.host_skills.body` and developer messages, a `## Skills` block with `### Skill roots` (`- \`r0\` = \`/abs/root\``) and `### Available skills` (`- name: description (file: r0/x/SKILL.md)`) | loaded entries are recoverable (18.2.2) |
| Skill roots: `REPO` `.agents/skills` in every dir from cwd up to the repo root, `USER` `$HOME/.agents/skills`, `ADMIN` `/etc/codex/skills`, `SYSTEM` bundled (plus `$CODEX_HOME/skills`, seen locally); host roots are scanned recursively (`loader/host.rs`: `SkillDiscoveryMode::Recursive` unless the root belongs to an Agent Plugin, whose `DirectChildren` mode stops at depth 2; `discovery.rs`: `MAX_SCAN_DEPTH` 6), directory symlinks followed for user/repo/admin scope, hidden dirs below the root skipped (checked on the walked path, not the link target), a `SKILL.md` that is itself a symlink not discovered; no per-session or per-repo gating of user skills | delivery is files (18.2.3): one directory link `~/.agents/skills/rematch → ~/.rematch/skills` exposes every `rematch/<name>/SKILL.md` (no per-entry links needed); the entry dirs hold real `SKILL.md` files |
| `SKILL.md` frontmatter is parsed into `{name, description, metadata}` without `deny_unknown_fields`; `$name` in user text mentions a skill (common env-var names excepted) | staged `SKILL.md` files (with the `rematch:` block) load unchanged; `$name` tokens (lowercase) become `skills_invoked` |
| `codex exec --json` prints `thread.started{thread_id}`, `turn.started`, `item.started\|updated\|completed{item}`, `turn.completed{usage}`, `turn.failed`, `error`; items are the thread items in snake_case (`command_execution{command: string, aggregated_output, exit_code, status}`, `file_change{changes:[{path, kind}], status}`, `mcp_tool_call`, `agent_message{text}`, `reasoning`, `web_search`, `todo_list`, `error`); no model name in the stream; `--ephemeral` writes no rollout | `parse_session` accepts this stream too (18.2.4) |

#### 18.2.2 Record mapping (`import_codex::trace_records_of`)

| Codex | Trace record (DESIGN §3) |
| --- | --- |
| `session_meta` + first model + skill list | `session_start`: `session_id` `codex:<id>`, `cwd`, `repo` (walk up to `.git`), `branch_hint`, `model`, `mode: "import"`, `agent` `main` / `sub` (`source.subagent.other`), `omp_version` = `codex <cli_version>`, `loaded_entries` = ids of the offered skills that are rematch's (the `SKILL.md`'s directory holds `rematch.json` — a link in the shared dir resolves to the staging or kept entry dir), skipping hook, agent and lesson entries (a hook's router is offered but the hook never runs in Codex) and entries created after the session started; `staging_dirs` = their directories |
| `UserMessage` (else `event_msg/user_message.message`, else response-item user message) | `input` with `redirect::prelabel` (mark > interrupt > revert > negation > reprompt), `after_deliverable` = last assistant text, `skills_invoked` from `$name` / `/skill:`; slash/`!`/`%` lines other than `/rematch mark` and `/skill:` dropped as in §3 |
| `CommandExecution` / `exec_command`, `shell` | `tool_result` tool `bash`, `input {command: <script>, cwd}`, `is_error` = status failed/declined or `exit_code ≠ 0` (response items: `Process exited with code N`, `Exit code: N`, legacy `metadata.exit_code`, `Script failed`), `path` / `skill_read` from a `parsed_cmd` read or `skills/<name>/SKILL.md` in the script |
| `FileChange` / `apply_patch` | `tool_result` tool `apply_patch`, `input {paths, changes}` (response items: `{input: <patch>}`, paths from `*** Update File:`), `path` = first file |
| `McpToolCall` | `tool_result` tool `mcp__<server>__<tool>`, `is_error` = failed, `error`, or `result.isError` |
| `ImageView`, web search | `tool_result` `view_image` (`path`), `web_search` (`query`) |
| `AgentMessage` | `turn_end` after the tool results that follow it: `model`, `usage` since the previous `turn_end`, `stop_reason` `stop` (final_answer) / `toolUse` (commentary), `text_head`; `model_change` first when the model changed |
| `task_complete` / `turn_aborted` | `agent_end {turns, yielded}` |
| end of file | `session_shutdown` with `trace::flag_reasons` (the extension's rule) |

`session_record` = `fold_trace` of that trace with `source: "import"` and the rollout as `source_path`, so the ledger row, `reflect --history`, `improve` and `count` agree on seqs. `import --history --harness codex` writes the synthesized trace of every clean main session with loaded entries (`traces_written`), so `count` folds it into those entries' counters; flagged sessions reach the counters through `reflect --history` (which writes trace + pending file). Real-history smoke (`REMATCH_REAL_HISTORY=1 cargo test -p rematch-core import_codex`): all 86 files parse; 17 reviewer threads; 352 inputs, of which the 217 in main sessions prelabel 137 none, 77 interrupt (inputs after an aborted turn, and mid-turn steers: 78 user messages arrived inside a running turn of one long Desktop session), 2 revert, 1 negation; 4064 tool results; 9 rows flagged.

Not recoverable from a rollout: what the model was shown beyond the skill list (rematch hooks never ran, so `hooks_loaded` is empty and no `tool_blocked`/`retrieval` record exists); `turn_end.usage.cost` (no price in the log); the provider-side model behind `model_provider` (`openai` unless configured); the skill set at session time when links changed before import (resolution reads the files at import time); calls inside a JS `exec` cell that are not thread items (`custom_tool_call exec` is one result in response-item files); `source` of an input (interactive vs. exec prompt) beyond `question_reply`; `idle` for a steer that Codex queued instead of injecting.

#### 18.2.3 Delivery: `skills sync --harness codex [--cwd <repo>]`

1. The shared sync of §16 "Shared skills": `REMATCH_ROOT/skills/<name>` for every kept/pinned entry and every provisional entry a synced cwd matched; the same links omp and Claude Code load. Codex reads them through the one link the user makes, `~/.agents/skills/rematch → ~/.rematch/skills` (or `<repo>/.agents/skills/rematch`, for one repo).
2. The envelope reports `reached_from`: which of `~/.agents/skills/rematch` and `<repo>/.agents/skills/rematch` resolve to the shared dir; with none, a line gives the `ln -s` that makes Codex see it.
3. Migration from the per-harness layout of older versions: a `rematch` link in those roots still pointing at `REMATCH_ROOT/codex-skills` is re-pointed (atomically) at the shared dir, and the kept copies under `~/.agents/skills/<name>/` marked `.rematch-codex.json` are removed (kept entries reach Codex through their links now); `uninstall` removes such copies too.

Codex has no per-session gating: every link is visible in every Codex session, whatever its repo, until its entry is retired. Counting stays exact (count-time predicate on the imported session), so a link visible in another repo costs context but never a counter.

#### 18.2.4 Batch mode (`run --harness codex`): plan, not built

Not built: the worker (`batch/worker.rs`), `Substrate::rollout_cmd` (returns `omp::ToolsCall`) and `run_rollout` (omp argv, `omp::verify_stream`) are omp-typed, and anchor replay reuses the omp binary; the change spans `run.rs`, `rollout.rs`, `substrate.rs`, `worker.rs`. The exact plan:

1. `RunArgs.harness: omp|codex` (default omp), carried in the job payload; `Substrate::rollout_cmd` returns `(program, argv, env)`; `OmpSubstrate` keeps today's argv.
2. `CodexSubstrate::materialize`: `git worktree add --detach <variant>/wt HEAD` as today; the effective tree written to `<variant>/wt/.agents/skills/<name>/` (repo scope, so only this rollout sees it) with `.agents/` appended to `<wt>/.git/info/exclude` (the worktree's `info/exclude` path from `git rev-parse --git-path info/exclude`) so verifiers diffing the worktree never see it; `HOME=<variant>/home` (empty, so `~/.agents/skills` contributes nothing) and `CODEX_HOME` left as is (auth lives there; its `skills/` and bundled system skills stay visible and are listed in the receipt as uncontrolled).
3. argv: `codex exec --json --ephemeral --cd <wt> --sandbox workspace-write --ignore-user-config -m <model> -- <prompt>` with `<model>` = `rematch-solver` resolved for Codex (`--model` on `run`, else Codex's default; recorded, not verifiable); env `REMATCH=0`, `REMATCH_SEED`; bound = task cap + 60 s through `proc::run_bounded` (codex has no `--max-time`).
4. Stream check (replacing `omp::verify_stream`): every non-empty line JSON, exactly one `thread.started`, ≥ 1 `turn.completed`, no `turn.failed` or `error` event; otherwise the rollout is invalid (verdict Fail "rollout invalid" without running the verifier), a time-capped run is still judged.
5. Trajectory: `import_codex::parse_session(stdout)` + `trace_records_of` (unit-tested on an `exec --json` stream), worktree path rewritten to the repo, `loaded_entries` = the materialized tree's ids, model = the `-m` value.
6. Verifier, receipts, next-run rule and anchor replay unchanged (anchor replay runs with the harness of the run that triggered it).

## 19. The lake (ADR 001)

History becomes a queryable backend: an append-only **journal** in the ledger records every write (`sessions`, `redirects`, `entries`, `counters`, `proposals`, `receipts`, `outcomes`) with the ledger's own clock as `known`; `rematch lake export` writes the journal to Parquet under `REMATCH_ROOT/lake/<table>/` (one Bellwether root per table, per-run files, never rewritten); `rematch lake index` builds Bellwether indexes under `REMATCH_ROOT/lake-index/<table>/` (per machine, never synced); `rematch lake query` plans through Bellwether and evaluates the residual in rematch. The hot path never reads the lake. Keys are declared only for questions a current reader asks (`report`, `why --asof`, `create --from sessions`, `improve`, `transfer suggest`); the rest of Bellwether's kinds are declined by name in the ADR with the question that would justify each. Identity that survives machines: `repo_remote` + subpath on predicates, `content_sha256` on entries, versioned `loaded_entries`. Full design, key table, bitemporal semantics, build plan L1–L4 and the review record: `docs/adr/001-lake.md`.
