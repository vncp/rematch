> Historical: this spec was written under the working name omp-evolve; the shipped project is **rematch**. Names in DESIGN.md are current.

# omp-evolve build spec v2

Oct 25, 2026 · @Vincent Pham

`omp-evolve` is an omp plugin that lets the agent improve its own harness from real use: it watches sessions, proposes skills, scripts, rules, agents and hooks grounded in execution traces, stages them in your personal `.omp/` as provisional, and keeps only what later sessions confirm. It starts from an empty library, creates new skills on request, improves existing ones from session history, and needs no benchmark to run. This tab is the build spec; the v1 tab holds the research it rests on.

**Principles**

1. The session is the evidence. No proposal exists without an execution trace behind it; no reruns in interactive use.
2. Verify with what is free: whether the redirect that motivated an entry recurs on later matched sessions, plus tool errors, tokens, and takeovers. Spend rollouts only in batch mode where a verifier exists.
3. All eight harness surfaces are editable; skills and scripts first, hooks and prompts last.
4. Provisional by default, kept by use, retired by counters. Nothing grows without an exit, and nothing is ever called "verified" on the strength of a handful of sessions.
5. Skill text activates in personal scope on its own. Anything executable (scripts, hooks), any subagent, any text added to the system prompt, and anything beyond personal scope waits for a one-tap approval or a merged PR.

## How you use it

Most of the time you don't. The plugin works from sessions you were going to run anyway, and the two asks you named are the only explicit entry points.

| Entry point | What you say or do | What happens |
| --- | --- | --- |
| Nothing | Work in omp as usual | Every session is traced by the extension into its own trace file. At the start of the next session, pending reflections run detached: a session that ended with a redirect, a tool-error cluster, or a takeover produces one bounded proposal grounded in that trace, staged as `provisional`. Later sessions whose trigger predicate matches load it. Counters decide whether it stays. |
| Session-start digest | Open omp | One line per entry staged since last time, with a diff on request. Skill text is already active; scripts, hooks, subagents and system-prompt fragments show a tap-to-approve, one item per entry (`kind` joins what is pending, e.g. `scripts+prompt`). Entries ready to ship to a team or fork get one `ship …` line, once. |
| Create a skill | "Use omp-evolve to create a skill for X" or `/evolve create <name> --from <sessions\|task>` | Greenfield: finds matching traced sessions, or runs one reference rollout of the task you name, runs the editor, lints, stages a skill package as `provisional`, reports the trigger predicate it chose. Description with no task and no sessions yields a `draft` that never loads until you activate it. |
| Improve a skill | "Based on session history, improve \<skill> using omp-evolve" or `/evolve improve <skill>` | Brownfield: registers the skill, audits where it loaded (from imported history and the trace), clusters the redirects and errors that happened with it in context, proposes unit-level edits, lints, stages a new version beside the old one. Refuses with a count if fewer than 5 traced sessions used the skill, and offers the mechanical split instead. |
| Batch job with a verifier | `/evolve run --task <id> --verifier <cmd>` | Harness² proper: reference rollout, two candidates, probe, compose, re-run; the verifier picks. Runs detached and returns a job id. |
| Look | `/evolve why`, `/evolve status`, `/evolve library` | Which entries were loaded for the last deliverable and why; what was staged, approved, kept, retired; counters per entry. |
| Correct it | `/evolve mark correction\|preference\|scope` | Overrides the redirect classifier for the last turn. Optional. |
| Stop it | `/evolve off`, `OMP_EVOLVE=0`, `/evolve disable <id>`, `/evolve rollback <skill>`, `omp-evolve uninstall` | Immediate. Uninstall removes every file the plugin wrote and nothing else. |

**What never happens without you:** a script becomes executable, a hook that can block an action is installed, a provisional subagent becomes visible, a fragment of text is added to your system prompt, the fork or team `.omp/` changes, a `pinned` skill is replaced, or an entry crosses from work to home. Each arrives as a one-tap approval in the session-start digest or as a PR with the evidence attached. An approved fragment is appended to the system prompt of main sessions where its entry loads; your own `AGENTS.md` is never written, and editing an approved fragment or subagent asks again.

**Rate limits.** At most one admission per trigger predicate per day, at most 10 `provisional` entries per branch, and no new reflection on a trigger while a canary for it is pending. A bad entry cannot cause a cascade of more entries.

**What the model sees:** provisional skills are ordinary omp skills contributed from a staging path, and an approved subagent is an ordinary omp agent; the model is not told it is being evaluated. The one addition to its context is the `## rematch: <name>` block of each approved prompt fragment, which you approved with its text in front of you. You are not blinded either, and the design does not pretend you are: the counters measure recurrence of a specific redirect class, which you would not fake.

## How it extends omp

Two pieces, the same shape compound-forge already proved in production: a TypeScript extension that omp loads, and a Rust binary the extension calls through `pi.exec` with a JSON envelope. No fork of omp is required to run it; the fork is one of the things it can propose changes to.

&#91;embedded content: omp-evolve v2 · extension, binary, two scopes\]

The extension only records and toggles arms; the binary proposes and stages; personal-scope entries flow back into the next session on their own, and anything beyond personal scope waits for a human merge.

**The extension** (`~/.omp/agent/extensions/omp-evolve/index.ts`, \~300 lines) registers:

| omp API | Used for | Verified against pi 0.80.9 docs? |
| --- | --- | --- |
| `session_start` | Open the trace file for this session; spawn `omp-evolve reflect --pending` detached (`unref`) over sessions flagged in the ledger; evaluate trigger predicates against repo and cwd | Yes |
| `resources_discover` | Contribute `~/.omp/evolve/staging/<branch>/` as an extra skills path when any provisional entry's predicate matches; this is how provisional skills load without touching `~/.omp/agent/skills/` | Yes: fires after `session_start`, receives `cwd`, returns `skillPaths`; additive. Caveat: fires per process, so a decision made at `session_start` holds until reload |
| `input` | Append each user turn with the deliverable it follows; cheap redirect pre-classification (takeover, revert, re-prompt, explicit mark); re-evaluate predicates on the first N turns | Yes |
| `tool_result` | Append a trace entry: tool, args hash, result size, error class. Skill activity is inferred from `read` calls on `skills/<name>/SKILL.md` and `/skill:` expansions; the event itself carries no "active skills" field | Yes (fields: `toolName, toolCallId, input, content, details, isError`) |
| `tool_call` | Run approved compiled verifiers; block with the rule's message | Yes |
| `session_shutdown` | Flush the trace and flag the session for reflection if it carried a signal. Nothing long-running starts here | Yes; async at shutdown is undocumented, hence the flag-then-spawn-next-start design |
| `registerTool("omp_evolve")` + `registerCommand("evolve")` | Entry points. Long operations start detached and return a job id; `status` polls; `ctx.signal` honored | Yes (`pi.exec` is buffered with a timeout, so it is used only for short calls) |
| `appendEntry` | Durable, out-of-context annotations: redirect labels, loaded-entry ids | Yes |
| `context` | Phase 2 only: retrieval of skill-package units within a token budget. Not used in Phase 1 | Yes, deferred |

Not used: `before_agent_start` to hide skills (it exposes the composed system prompt and a message, not a per-skill toggle, and the skill directory would still be readable), and `tool_result` as the place to learn which skills are active.

**The binary** (`omp-evolve`, Rust, one static executable) owns everything that is not per-event: reading the extension's trace files, importing historical omp session JSONL, redirect classification (model tier, through omp), the editor session (through omp with `--no-tools`, inputs in-prompt, as the forge's runner already does), lint, the library, counters, staging, batch-mode recursion, PR output, and the ledger. It never holds provider credentials. The schema lives in Rust only: the extension forwards raw event JSON and parses nothing, which is what keeps the two-language split from costing a mirrored type system. If that rule ever breaks in practice, the reviewer's alternative stands: fold the binary into the TypeScript package.

**Files it writes**

| Path | Contents | Who reads it |
| --- | --- | --- |
| `~/.omp/evolve/traces/<session>.jsonl` | The extension's own trace: raw events as JSON lines | Binary; independent of omp's session format |
| `~/.omp/evolve/ledger.sqlite` + `blobs/` | Sessions, redirects, proposals, probes, receipts, counters, predicates | Binary; `/evolve why` |
| `~/.omp/evolve/staging/<branch>/<entry>/` | Provisional skill packages; scripts inside are mode 0644 until approved | omp, through `resources_discover` when the predicate matches |
| `~/.omp/agent/skills/<name>/` | Kept entries, copied in after the counters decide; frontmatter `evolve: {status, branch, family, id}` | omp, natively |
| `~/.omp/agent/extensions/evolve-hooks/<id>.ts` | Compiled verifiers, only after approval | omp, natively |
| `<repo>/.omp/**` via PR | Team-scope entries | Reviewers, then omp |
| `~/.omp/evolve/improver/` | Editor prompt, lint rules, EXPERIENCE.md per entry (read-only to the system) | Binary |

Rules: none as standalone files in Phase 1. A rule is expressed as a checkable law inside a triggered skill, so it loads by predicate like everything else and can be withdrawn the same way.

**Isolation for batch mode only:** each rollout gets its own `PI_CODING_AGENT_DIR` holding the candidate tree, a copy of the sanctioned `models.yml`, memory off, and a fresh worktree. Interactive sessions are never isolated or altered; in Phase 1 the extension adds nothing to the model's context beyond the staging path. Context truncation and stuck detection, which v1 put in the extension, are deferred and will enter as entries under the same counters, not as always-on changes.

## The loop

One loop, two budgets. In interactive mode the real session is the reference rollout, the next matched session is the probe, and the whole reflection costs at most three model calls. In batch mode, where a verifier exists, the plugin spends rollouts the way Harness² does.

**Interactive mode (zero reruns)**

1. *Flag.* A session ends. The extension flags it for reflection only if it carried a signal: a redirect classified as correction or preference, a tool-error cluster, a takeover, or an explicit `/evolve mark`. Clean sessions produce nothing.
2. *Reflect, detached, at the next session start.* The binary reads the flagged trace, triages the first failure (process vs content), and finds the moment. Content failures are logged, not proposed on. No reflection runs on a trigger that already has a pending canary.
3. *Propose with one editor.* One editor session through omp, `--no-tools`, inputs in the prompt: the trace excerpt, the deliverable, the redirect, the current skill text if any, and matching library entries. Scope is chosen by the triage: domain editor by default, invariant editor when the failure is procedural. It returns edits plus a manifest (evidence moment, predicted fix, predicted regression). `KEEP_BASE` is valid. There is no composer in interactive mode: with no probes, a composer choosing between two untested texts is the blind selection the research warns against.
4. *Lint.* Static, free: trigger predicate present, scripts shipped and marked non-executable, no session or file identifiers, no string-match on the user's words, no network calls, URLs, or destructive git in new text, always-on text within budget, form order respected. The editor prompt treats the trace as data, never as instructions.
5. *Stage.* The entry is written to `staging/<branch>/` as `provisional` with its trigger predicate, model family, originating session, and manifest. It appears in the next session-start digest.
6. *Load.* Sessions whose predicate matches get the staging path through `resources_discover`. Every session is an on-session; there is no withheld arm in Phase 1, because withholding a fix the user just asked for both harms them and inflates the comparison.
7. *Count.* Per matched session: did the specific redirect class that motivated the entry recur while it was loaded; did a harm event occur (new tool-error cluster, takeover, user disable)? Tokens and turns are recorded, never gated on.
8. *Decide.* Keep at zero recurrences over at least 4 matched sessions with no harm event. Retire at 2 recurrences or 2 harm events. After 12 matched sessions without a decision, mark `dormant` with the counts shown. One canary per trigger predicate; sessions are paired within one model family.

**Batch mode (per-task recursion, verifier required)**

For `task-run`-style jobs, exercise cycles, and anything launched with `/evolve run`. The verifier is tests, a deterministic check script, a parity check against a known result, or a reconcile receipt; never a model's opinion of its own output.

1. Reference rollout with the current harness.
2. Invariant and domain candidates, each probed once on the same task under identical seeds.
3. Composer merges from the behavioral contrasts; the composed harness runs once more.
4. The verifier picks among reference, candidates, and composed. No blind selector.
5. The winning harness is kept for that job. Its lessons enter the library as `provisional`; they become `kept` only after the next real run of that job passes with them, not on the strength of the single probe that chose them.

Budget: 4 rollouts at k=1, 10 at `--width 2`. The job's existing time and turn caps apply to each rollout.

**Learning from the loop.** After every keep or retirement, the reflector writes an `EXPERIENCE.md` beside the entry: situation, lesson, evidence, and what the counters showed. The editor reads matching ones on later proposals, which is Harness²'s library in its simplest form. The editor prompt itself is not amended by the system; a self-amending improver measured by its own admission rate is the evaluator evolution this plan excludes.

**Hard rules.** No proposal without a trace. No activation without lint. No executable content, subagent or system-prompt text without approval. No `kept` status without later sessions or a verifier run after the one that chose it. No rerun inside an interactive session. At most three model calls per reflection.

## Starting states

The same loop serves all three; what differs is where the reference trajectories come from.

**Empty library.** Nothing is installed except the extension and the binary. The first thing `omp-evolve import --history` does is read omp's existing session JSONL and build the ledger's record of past sessions, including which skills were read in each (inferred from `read` calls on `skills/<name>/SKILL.md` and `/skill:` expansions). From then on the extension writes its own traces. Nothing is proposed until a signal appears, which in practice is the first redirect; the first gain arrives on the next session that matches the entry's predicate. The editor prompt and lint rules ship with the binary, so an empty library is not an empty improver.

**Trigger predicates.** Every entry carries a deterministic predicate the binary writes when it stages the entry: a repo glob, a set of file extensions touched, and up to 10 keyword stems taken from the originating turn and the files it touched. The extension evaluates it at session start against repo and cwd and again on the first few turns against inputs and files read. A session counts for the entry only on a predicate hit. Skill trigger text stays for the model; the predicate is for the counters.

**Greenfield: "create a skill for X."** Three sources of reference trajectories, used in this order:

1. *Sessions that already match X.* The binary searches the ledger (imported history plus its own traces) for sessions whose predicate-style features match the request. With three or more, those are the references: the skill is distilled from what worked and what was redirected, facts labeled as workspace facts, procedure written for the domain.
2. *One reference run.* With no matching sessions and a concrete task, the binary runs it once in batch mode (one rollout, no candidates) to obtain a trace, then proposes from it. This is the only create that costs a rollout.
3. *Description only.* With neither, the editor writes from the description and any files you pointed at, and the result is staged as `draft`: visible in `/evolve library`, never loaded. It becomes `provisional` when you give it a task to run once (`/evolve activate <name> --task`) or explicitly accept the risk (`/evolve activate <name> --unverified`). This keeps the no-trace rule intact.

The result is a skill package: a short router `SKILL.md` with the trigger line and the checkable laws, `units/` for procedures and pitfalls, `facts/` for measured values with `as_of`, and `scripts/` for anything deterministic. For a small skill the package collapses to one `SKILL.md`; the package layout appears only when a skill crosses about 8 KB.

**Brownfield: "improve \<skill> from session history."**

1. *Register.* The existing skill becomes a `provisional` entry with no counters and a predicate derived from its trigger text and the sessions that read it.
2. *Audit where it loaded.* From imported history and traces: which sessions read it, which redirects and errors happened with it in context. Attribution is at the file level until the skill is a package; the trace cannot see units that do not exist yet. If fewer than 5 traced sessions used the skill, `improve` says so and offers only the mechanical split.
3. *Split first, if large.* For a skill over 8 KB the first improvement is mechanical and needs no model: router with the trigger and the laws, `units/` by heading, `facts/` for measured values with `as_of`, long tables and rejected-hypothesis lists into the ledger. Later improvements are unit-level.
4. *Propose at unit level,* with the single editor and the same lint, from the clustered redirects: a pitfall rewritten as a checkable law, a procedure moved to a script (staged non-executable), dead text removed.
5. *Stage beside, not over.* The new version is a sibling entry under the same predicate; the old one stays until the counters decide. A `pinned` skill is never replaced automatically.

No coverage quiz: generating a prompt per rule and checking that each rule changes model behavior is the decision-suite benchmark this plan excludes, and at 50 rules it would cost more model calls than a month of reflections.

**Importing compound-forge.** `omp-evolve import --forge <path>` registers all 15 skills as `provisional` brownfield entries with predicates derived from their trigger text, and turns each `learnings/*.md` into a `provisional` lesson with its evidence hash. The eval suites are kept as files for reference and not used as a signal.

## Session outcome records

Sessions should leave behind one structured record of what they produced, in a schema that is the same for every domain, so the ledger can be queried across domains and a win in one can be tried in another. Past sessions cannot be retrofitted beyond what the importer extracts deterministically; this is the hint to future sessions.

**Who writes it.** The extension fills the deterministic fields at session end (repo, files changed, tools, errors, redirect classes, tokens, model). Where the domain already emits a machine-readable receipt (the quant engine's validation receipts, test reports, a bellwether receipt), the record is derived from it with no model call. Only when a claim exists in prose and nowhere else does the reflector fill it, and those fields are marked `self_reported`. A self-reported field is evidence about what the agent believed, not about what happened; it never promotes an entry on its own.

**The schema** (fixed core, per-domain facets in the same columns)

```text
Outcome {
  id, session_id, ts, machine, model_family, repo, branch
  kind:      change | hypothesis | decision | artifact | lesson
  subject:   { domain: "quant.options" | "repo.payments" | "3d.animation",   // dotted path, prefix-seekable
               entities: [ {type, id} ] }                                      // strategy, file, module, dataset, exercise
  claim:     one sentence: what was tried or changed
  signature: hash(sorted normalized params)                                    // exact key: "same thing"
  params:    { k: v }                                                          // normalized
  facets:    [ {key, value} ]                                                  // domain vocabulary, e.g. mechanism, universe, horizon
  evidence:  [ {kind: test|receipt|review|redirect|metric, ref, verdict} ]
  result:    { status: kept|killed|inconclusive|shipped|reverted, metrics, kill_criterion? }
  links:     [ {rel: supersedes|depends_on|contradicts|transfers_from, id} ]
  provenance:{ self_reported: [field...], data_snapshot, tool_versions }
}
```

**Why facets instead of an ontology.** A global schema for options, semiconductor hypotheses, repo changes, and animation work would be wrong for all of them. The core stays fixed; each domain registers its facet vocabulary in a `domain.yml` inside its branch (quant: mechanism, universe, horizon, data\_snapshot; repo: module, change\_type, test\_scope; 3D: rig, exercise, bake). Facets live in one `(key, value)` column pair, so a cross-domain query is a prefix seek on `(domain, key, value)` and needs no join.

**Three lookups, three keys.** Same thing: the exact `signature`, which is the gate a proposer must query before proposing a hypothesis or change. Close thing: MinHash over the normalized token set, a suggestion never a gate. Related thing: embeddings, exploratory only. An estimator may suggest; it never decides.

**Storage.** `outcomes` is a table in the lake beside the session events, partitioned by date and sorted by `subject.domain`, with keys `exact:signature`, `composite:(domain, facet_key, facet_value)`, and `temporal:subject_id@ts` so `latest` answers "what is the current status of this hypothesis" and `asof` answers what was known on a date. The ledger keeps only the latest status per id.

**Cross-domain transfer, as a mechanism rather than a hope.** A kept lesson is tagged invariant (about process: verification, decomposition, caching, retrieval) or domain (about facts) by the reflector, the same split the editors use. `omp-evolve transfer suggest` lists kept invariant lessons not yet tried in a branch whose facet structure overlaps on two or more structural keys (append-only data plus repeated key lookups is how session history matched a data lake). Suggestions appear in the digest; a transfer is applied as `provisional` in the target branch and earns its place through that branch's counters. Domain facts and workspace facts never transfer; lint blocks them.

**What the importer can and cannot recover.** From old sessions: files changed, tools and errors, skill reads, and `Outcome` records wherever a receipt exists. Not recovered: claims that live only in prose. The 5 TB stays queryable as events; it becomes a research log only going forward.

## Library and admission

The library is the set of entries in personal scope plus the ledger that tracks them. It is bounded by statuses with exits, caps that force merges, and a scope rule for what needs a human.

**Statuses**

| Status | Means | Enters | Leaves |
| --- | --- | --- | --- |
| `draft` | Visible, never loaded | Created from a description with no trace | `provisional` when you give it a task or accept it unverified |
| `provisional` | Loaded on predicate match from the staging path; scripts non-executable | Lint passed | `kept` or `retired` by the counters; `dormant` after 12 matched sessions without a decision or 30 days with fewer than 3 matches |
| `kept` | Copied into `~/.omp/agent/`; loaded on every match | 0 recurrences over ≥ 4 matched sessions, no harm event; or a batch verifier picked it and the next real run passed | `retired` at 2 harm events in 30 days; `stale` on evidence drift |
| `stale` | Excluded | A fact's source snapshot, the model family in use, or the fork commit changed | `provisional` again (new canary) or `retired` |
| `dormant` | Excluded, kept | Too few matches, or no decision | `provisional` when its predicate starts hitting again |
| `retired` | Archived with evidence; never loaded, never deleted | Counters, approval declined, or `/evolve disable` | Never |
| `pinned` | Human-owned | You set it | You unset it |

`kept` is deliberately not called `verified`. Four matched sessions with no recurrence is evidence that the entry did its one job, nothing more.

**Counters.** Each entry carries `recurrence` (the specific redirect class that motivated it happened again while it was loaded), `harm` (a new tool-error cluster, a takeover, or a user disable while it was loaded), and `matched` (sessions where its predicate hit). The counts are raw and shown raw in `/evolve library`. Tokens and turns per matched session are recorded for `why`, never gated on, because they vary tenfold with task difficulty. A model never rewrites counters.

**Caps and rates.** Per branch (a repo or project): at most 10 `provisional` entries, at most one admission per trigger predicate per day, and 5 `kept` entries of the general kind plus 5 per branch before a new candidate must merge or revise an existing one. Near-duplicates (simhash over normalized text, identical script bodies) route to the existing entry with evidence appended.

**Lint, in full.** No session ids, task-specific file paths, or user names in the text. No string-match branch on the user's words. Every skill carries a trigger predicate and a trigger line. Every referenced script is shipped, mode 0644, and contains no network call, URL, `curl | sh`, or destructive git (`push -f`, `reset --hard`, `clean -fd`) unless the manifest explains it and approval is requested. Always-on text within 8 KB per branch, new text paid by deletion. Form order respected unless the manifest says why not. Workspace facts carry an `as_of`. A hook has a documented pass condition and cannot block a valid completion.

**Scope and who merges**

| Scope | Activation | Examples |
| --- | --- | --- |
| Personal, text | Automatic as `provisional` | Skill text, laws, facts, agent profiles |
| Personal, executable or blocking | One tap in the session-start digest, with the diff | Scripts, compiled `tool_call` verifiers |
| Team (`<repo>/.omp/`) | PR with evidence | Entries whose predicate recurred across 2+ repos, or that you choose to share |
| Fork | PR with built fingerprint and before/after numbers | Per-model prompt text, tool descriptions, compaction defaults |
| Work → home export | Per-entry approval with the diff shown; off by default at work | Invariant entries only; lint cannot see domain idioms learned from work traces, so a human must |

**Branches.** A branch is a repo or project, set from the session that produced the entry. `shared` holds entries kept in two or more branches; promotion needs a second branch's canary, which costs nothing.

**Rollback and anchors.** Every kept entry keeps its predecessor; `/evolve rollback <skill>` restores it immediately and retires the current one with a note. Before a sibling replaces a kept entry, its originating task is replayed once in batch mode if it has a verifier; otherwise the sibling's own canary is the anchor. There is no separate anchor suite.

## Models

One library, two environments, model-aware bookkeeping. The evidence says harnesses transfer partially across models (a harness tuned for Gemini 3.5 Flash gave GLM-5 +17.3 on code, Opus 4.8 +2.6, Gemini 3.7 Flash +0.3), that same-model editing works for every model tested, and that weaker models gain most and can be hurt most. So the design keys on model family without duplicating the system.

| Role | Work (Sol, Argon) | Home (Anthropic and others) | Rule |
| --- | --- | --- | --- |
| Solver | Whichever you are using | Whichever you are using | Every entry records the family it was created and kept under |
| Editor | Strongest configured: Sol when available, else the solver's own model | Strongest configured, typically Opus | omp `modelRoles` gets an `evolve-editor` role; the binary passes `--model` explicitly and verifies provider and model on every event. Same-family editing is acceptable and is recorded; a different provider than configured is a failed run, never a fallback |
| Redirect classifier | A small or fast role | Same | Offline in the binary, batched |

No judge role: interactive mode has no selection step and batch mode requires a verifier.

**Record family, don't gate on it.** Entries carry `family` for the session that created them and a per-family breakdown of the counters. They load regardless of family, because gating would make entries flicker as you switch Sol and Argon day to day. The split only happens when the counters diverge: harm on one family and clean on another splits the entry per family, and the weaker family's variant is proposed separately. Per-family prompt variants as a standing mechanism are dropped; prompt text was the surface with the least measured transfer and the least measured value.

**Home and work are separate libraries** under separate `~/.omp/evolve/` roots, with separate data policies. Invariant entries (how the agent works, not what a repo needs) sync between them through a git-tracked export, arriving as `provisional`. Domain entries stay where they were born.

**Capability floor.** A family whose recurrence or harm counts rise across several entries in one window triggers a review list in the digest rather than automatic retirements; weaker models are the ones both most helped and most hurt by harness changes.

## Components and files

```text
omp-evolve/
  Cargo.toml
  crates/
    evolve-core/        trace model + parsers (own trace, omp JSONL import), redirect heuristics,
                        predicates, lint, library + counters, ledger (SQLite + blobs), package model
    omp-evolve/         binary: CLI, reflect/create/improve/run, editor channel (omp -p --no-tools),
                        batch scheduler, staging, PR output, import, uninstall
  extension/
    package.json        "omp": { "extensions": ["./index.ts"] }
    index.ts            events -> trace file, predicates, resources_discover staging path,
                        digest, omp_evolve tool, /evolve command; parses nothing
  improver/
    editor.md, reflector.md, lint.toml      (ported from Harness² Appendix H; no composer or selector)
  tests/
    goldens/            omp session JSONL per tracked release, for the importer only
```

**Core types** (illustrative)

```rust
struct Entry { id, kind: Skill|Rule|Script|Agent|Hook|Fact|Lesson, branch, scope,
               status, verified_on: Vec<Family>, helpful: u32, harmful: u32,
               evidence: Vec<Moment>, origin: Session|Task|Import|Description }
struct Moment { session_id, seq }                   // the unit of evidence everywhere
struct Proposal { edits: Vec<Edit>, manifest, cites: Vec<Hash> }   // has no score field
struct Certificate { lint: LintReport, canary: Option<Paired>, verifier: Option<Verdict> }
struct Receipt { certificate: Hash, entry: EntryId, activation: Personal|Prompted|Pr }

trait Substrate   { fn materialize(&Tree, &Path); fn rollout_cmd(&Install, &Task, seed) -> Command;
                    fn read_trajectory(&Path) -> Vec<TraceEntry>; }
trait EditorChannel { fn run(role, workspace, budget) -> EditorResult; } // verifies provider+model per event
trait Verifier    { fn check(&Task, &Worktree) -> Verdict; }            // tests, script, parity, receipt
```

**CLI**

```text
omp-evolve doctor                        omp flags, editor role, extension loaded, ledger schema
omp-evolve import --history [--forge]    omp session JSONL -> ledger; forge skills and learnings -> provisional
omp-evolve reflect --pending             detached at session start; idempotent per flagged session
omp-evolve create <name> --from sessions|task [--branch]      (description only -> draft)
omp-evolve improve <skill> [--split-only]
omp-evolve activate <name> [--task <id> | --unverified]
omp-evolve run --task <id> --verifier <cmd> [--width k]       detached; returns a job id
omp-evolve status [job]
omp-evolve library {ls, why <deliverable>, disable <id>, rollback <skill>, pin <id>, approve <id>, export}
omp-evolve ship --scope team|fork
omp-evolve off | on
omp-evolve uninstall                     removes every file the plugin wrote, nothing else
```

**Where state lives**

| State | Location | Notes |
| --- | --- | --- |
| Traces | `~/.omp/evolve/traces/` | Written by the extension from events; the only live input to reflection |
| Ledger | `~/.omp/evolve/ledger.sqlite`, `blobs/` | One per environment |
| Staging | `~/.omp/evolve/staging/<branch>/` | Provisional entries; contributed via `resources_discover` on predicate match |
| Kept entries | `~/.omp/agent/{skills,agents,extensions}/` | Copied in on keep; frontmatter carries the evolve block; predecessor retained for rollback |
| Improver | `~/.omp/evolve/improver/` | Editor prompt, lint rules, EXPERIENCE.md files (read-only to the system) |
| Batch workspaces | `~/.omp/evolve/runs/<task>/<seed>/` | Deleted after the receipt is written |
| Team and fork outputs | A git branch in the target repo | Opened as a PR by `ship`; never merged by the binary |

**Build and install.** `cargo build --release` for the binary; the extension is plain TypeScript omp loads with no build step. `omp plugin install @<org>/omp-evolve` installs the extension and places the binary on `PATH`; `omp-evolve doctor` confirms the fork's flags for JSON event streaming and the editor role. The forge's `omp_runner` contract (separate argv, per-event provider and model verification, process-group cancellation, bounded time) is the binary's `EditorChannel`.

**Tests.** Golden-file tests for the trace parser against the sessions in `tests/goldens/`; property tests for the context policy; fixture tests for lint and for canary decisions including the no-decision case; one end-to-end test that runs reflect → propose → lint → stage against a scripted editor.

## Build plan

Three phases, each usable on its own. Phase 1 is the whole interactive loop; if it does not earn its keep in daily use, nothing after it is built.

| Phase | Weeks | Delivers | Done when |
| --- | --- | --- | --- |
| 1 · Reflect and count | 1–3 | Extension: trace file, redirect capture, predicate evaluation, `resources_discover` staging path, digest, `omp_evolve` tool and `/evolve`. Binary: `import --history`, `reflect --pending` with one editor, lint, staging, counters, `why`, `off`, `disable`, `rollback`, `uninstall`. Editor prompt ported from Harness² | A redirect in a real session produces a provisional entry; a later session whose predicate matches loads it from staging; `/evolve why` explains it; `/evolve off` stops everything |
| 1.5 · Create and improve | 4–5 | `create` (sessions, task, draft), `improve` (register, audit, mechanical split, unit edits), `import --forge`, `approve` for scripts | A greenfield skill is created from three matching sessions; the 31 KB quant skill is split mechanically and runs as a package; the forge's 15 skills are registered |
| 2 · Batch mode | 6–8 | `run` with verifier, per-task recursion, worktree isolation, the next-run keep rule; context-hook unit retrieval; context truncation and stuck detection as entries | One `task-run` job completes a k=1 cycle, the verifier picks, and the next run keeps or retires it |
| 3 · Scope and transfer | 9–11 | `ship` for team and fork PRs; work→home export with approval; `stale` on drift; split-per-family when counters diverge | One team PR merged from evidence; one entry promoted to `shared` through a second branch |

**Kill criterion for the project.** After six weeks of daily use: if no entry has reached `kept` over at least 4 matched sessions, and recurrence has not fallen on any trigger the library covers, stop at Phase 1 and keep stock omp with hand-written skills. The trace tooling and `why` stay useful either way.

**What is deliberately not in the plan:** a benchmark harness, a noise-floor suite, statistical margins, evaluator evolution, and an indexing layer beyond simhash dedup and trigger keys. Each is in the v1 tab for the day the loop proves it needs them.

## Architecture review

An independent reviewer with no view of the design history read the v2 draft against the pi 0.80.9 extension docs and the stated goals. Verdict on the draft: "rethink the core mechanism, then ship a smaller Phase 1." Two blockers and most majors were accepted and are reflected in the sections above; the table records each finding and what was done.

| # | Severity | Finding | Resolution |
| --- | --- | --- | --- |
| 1 | Blocker | The on/off arm had no mechanism: `before_agent_start` exposes the composed prompt, not a per-skill toggle, and the skill directory stays readable on the off arm | Replaced. Provisional entries live in a staging path contributed through `resources_discover` on predicate match. No off arm in Phase 1 |
| 2 | Blocker | "Matching" was undefined while every counter and the kill criterion depended on it | Added a deterministic trigger predicate per entry (repo glob, file extensions, ≤ 10 keyword stems), evaluated identically for every session |
| 3 | Blocker | Description-only greenfield contradicted the no-trace rule and activated untested text | Description-only yields `draft`; it loads only after a task run or an explicit unverified activation |
| 4 | Major | The interactive composer was blind selection between two untested texts, at 4–30 calls per reflection | One editor, `--no-tools`, in-prompt inputs, ≤ 3 calls per reflection, daily cap, no re-reflect on a pending trigger |
| 5 | Major | Brownfield coverage check was a quiz benchmark at \~150 calls per large skill | Cut. Onboarding is register, audit from history, mechanical split |
| 6 | Major | Brownfield had no data on day one; omp's JSONL does not record active skills | `import --history` infers skill reads from `read` calls and `/skill:` expansions; `improve` refuses below 5 traced sessions and offers the split |
| 7 | Major | 3-vs-3 cannot decide anything; tokens and errors vary tenfold by task; the user is unblinded | Primary signal is recurrence of the motivating redirect class; keep at 0 over ≥ 4 matched sessions, retire at 2; raw counts shown; status named `kept`, never `verified` |
| 8 | Major | The off arm withholds a fix the user asked for and inflates its own comparison | On-only canaries. Alternation reserved for always-on text in Phase 2, if ever |
| 9 | Major | Batch k=1 winner marked `verified` reproduced single-rollout acceptance | Batch winners are `provisional`; `kept` after the next real run passes with them |
| 10 | Major | Auto-activated model-written scripts were unreviewed code execution; textual lint would pass `git push -f` | Scripts staged mode 0644 until a one-tap approval; lint rejects network calls, URLs, destructive git; editor treats the trace as data; session-start digest with diffs |
| 11 | Major | No kill switch, rollback, rate limit, or uninstall | All added: `/evolve off`, `OMP_EVOLVE=0`, `disable`, `rollback`, per-predicate daily admission cap, 10 provisional per branch, `uninstall` |
| 12 | Major | Background reflect at `session_shutdown` is unreliable under `pi.exec` | Flag at shutdown; spawn `reflect --pending` detached at the next `session_start` |
| 13 | Major | Live path depended on omp's session JSONL format | The extension writes its own trace file; omp JSONL is read only by the historical importer |
| 14 | Major | Phase 1 was 6–8 weeks of work labeled 3 | Phase 1 cut to reflect, lint, stage, count, why, safety controls; create and improve moved to 1.5 |
| 15 | Major | Rust buys nothing for a binary that parses JSON, writes SQLite, and spawns omp; types get mirrored in TS | Partly accepted. Rust is kept as the owner's stated requirement, with the rule that the schema lives only in Rust and the extension forwards raw events. The single-package TypeScript alternative is recorded as the fallback if that rule breaks |
| 16 | Major | `tool_result` has no "active skills" field | Activity inferred from skill-file reads; attribution is file-level until packages exist; stated in the spec |
| 17 | Major | Context truncation and stuck detection were unverified always-on changes that confound the counters | Removed from Phase 1; they enter later as entries under the same counters |
| 18 | Major | Editor self-amendment measured by admission rate is Goodhart-able and is evaluator evolution | Cut. EXPERIENCE.md is read-only to the system |
| 19 | Minor | Judge role had no caller; per-family prompt variants were maximal complexity for the smallest gain; `verified_on` gating would make entries flicker between Sol and Argon | Judge and prompt variants removed; family recorded, not gated; split only when counters diverge |
| 20 | Minor | Work→home export of "invariant" entries could leak work context lint cannot see | Per-entry approval with the diff; off by default at work |
| 21 | Minor | Kill criterion could not complete a 6-session window on a weekly trigger | Rewritten around 4 matched sessions or a recurrence drop |
| 22 | Minor | Long `omp_evolve` operations under buffered `pi.exec` block the UI | Detached jobs with ids; `status` polls; `ctx.signal` honored |
| 23 | Minor | `main` collides with git naming; "2+ users" unreachable solo; "domain" undefined for caps | `shared`; "2+ repos or users"; caps are per branch |

**Not accepted in full:** finding 15 (language choice), for the reason above.

**Reviewer's three additions, all taken:** kill switch and rollback set; stored deterministic predicate plus historical importer; session-start digest with diffs and tap-to-approve.

## Open questions

- [ ] Confirm in the fork that `resources_discover` is re-run on `/new` or session switch, not only at process start; if not, the staging path is decided once per process and the digest says so.
- [ ] Confirm `--mode json -p` remains the non-interactive event stream in the fork (compound-forge's runner relies on it today).
- [ ] Does the fork expose `modelRoles` to extensions so `evolve-editor` can be read, or does the binary read `config.yml` directly?
- [ ] Predicate shape: are repo glob + file extensions + 10 keyword stems enough to separate triggers in a monorepo, or is a path prefix needed?
- [ ] Team-scope consent model for session mining at work: personal only, or pseudonymized team scope?
- [ ] For quant batch jobs, which verifier ships first: parity against the deployed book, or the planted-truth synthetic generator?
