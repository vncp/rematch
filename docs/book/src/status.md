# Status and limits

Built: the interactive loop (Phase 1), create and improve with the mechanical split and the forge import (Phase 1.5), batch mode with worktree isolation and the next-run keep rule, context-hook unit retrieval, and tool-output truncation and stuck detection as entries under the same counters (three built-in hook entries that wait for your approval; Phase 2), team and fork shipping, work-to-home export, stale on drift, split per family, transfer suggestions and `shared` promotion (Phase 3), and omp profile resolution (`OMP_PROFILE` / `PI_PROFILE` / `PI_CONFIG_DIR` / `PI_CODING_AGENT_DIR`, multi-profile history import with `--skip-profile` and `policy.json` `skip_profiles`).

Harnesses: omp is the reference. Claude Code runs the loop live through `adapters/claude-code` (see [Harnesses](get-started/install.md#harnesses)); checked with `claude -p --model haiku` (Claude Code 2.1.285) against a temporary root: `session_start`, `input`, `tool_result` (a failing `Bash` call as `is_error`), `turn_end` with the model, `agent_end` and `session_shutdown` traced; a negation in a resumed session pre-labelled, flagged and counted once, whole; `reflect --pending` started detached by the next `SessionStart` (it counted the session and attempted the editor, which the temporary root had no model role for); a prompt queued while a `Bash` call ran traced as `interrupt`; the digest with approval commands delivered at `SessionStart`; a provisional agent copied and listed by `claude plugin details`, then counted in the next session. Not yet observed live: the digest's display in an interactive session, an Esc abort (covered by an end-to-end test on the transcript format), and a Claude Code session reflected by a real editor model. Codex is post-hoc only.

Lake (ADR 001), L1–L4 built. L1: every ledger write is journaled with the ledger's own clock; entries carry `content_sha256`, sessions and predicates `repo_remote` + `subpath`, traces versioned `loaded_entries`. L2: `rematch lake export` writes the journal and every closed session's trace to Parquet under `~/.rematch/lake/` (per-run files, never rewritten — except that `lake index` upgrades an events file exported before the stored `tool_cmd` column, once —, byte-identical for the same journal; also at the end of every `reflect --pending`), `lake import` merges a second machine's lake, `lake rebuild` reproduces the ledger (`library ls` and counters exactly) on a fresh root, `lake doctor` reports watermarks, lag and stray files. L3: `rematch lake index` builds one Bellwether index per table under `~/.rematch/lake-index/` with the ADR 001 key table (all 15 specs accepted by bellwether 0.1.0, among them `exact:tool_cmd` and `text:tool_cmd,tokenizer=words` on the events' bash command; the append-table path template uses `{run_seq:int}` because Bellwether refuses a `{seq}` placeholder that disagrees with the stored per-row `seq`), pins the Bellwether version in `lake/manifest.json` (`--repin` to rebuild under another), and `rematch lake query <table> --key … --values …` (a text key: `--has word,word`, one `locate --has` per word, row groups intersected) plans through `locate|latest|asof`, evaluates the returned residual (in, between, not_older_than, latest_per_key, text, contains_any/all; any other kind refused by name) and returns rows as JSON — with no binary or no index it scans every file and says `indexed: false` with the same rows. Details: DESIGN §19.2. L4: `reflect --history` and `improve` read a session's records from its events file, `create --from sessions` and `transfer suggest` read every machine's `sessions` and `outcomes` (`create --ran cargo,test` the events' `text:tool_cmd`), `library why <session> --asof <ts>` shows each loaded entry's status and each redirect's class as they stood then, each naming its source (`lake` or `ledger`, the ledger while the lake lags it); `rematch report` computes the kill criterion — entries kept over ≥ 4 matched sessions, recurrence per trigger on sessions with an entry of it loaded against sessions without, and the verdict line — from the lake alone, or from the ledger's journal without one (DESIGN §19.3).

The lake is verified by the test suite on temporary roots: two exports byte-identical, a second machine's lake imported, rebuild reproducing `library ls`, `why --asof` returning the status before a transition, and lake and ledger agreeing for every switched reader and for `report`. It has not yet been run against this machine's real `~/.rematch` history, indexed with Bellwether over it, or combined with a second real machine's lake.

Profile built: `rematch profile init|adopt|sync|status|install-launchd`, keeps and hook approvals written into `~/.rematch/profile/`, views linked into the agent dir, `roles.yml` ahead of `config.yml`, the background sync at omp session start, conflicts in the digest, `uninstall` keeping the repo. Verified by `crates/rematch/tests/e2e_profile.rs` on two temporary machines sharing a local bare remote (adopt, clone, a keep crossing machines, a conflict left clean, an unreachable remote reported offline in about 2 s, roles per machine, uninstall); not yet run between two real machines or against a hosted remote.

Not yet earned:

- **A `kept` skill from daily use.** The three built-in hooks reached `kept` (9/6/5 matched sessions, 0 recurrence, 0 harm) during the build itself; no skill has yet. The kill criterion in the spec is six weeks of daily use: if no entry reaches `kept` over at least four matched sessions and recurrence has not fallen on any trigger, stop at Phase 1 and keep stock omp with hand-written skills. The trace tooling and `why` stay useful either way.
- **The built-in context entries in real use.** All three were approved and fired in one live omp session on this machine (a `retrieval` record for a `bellwether-dev` unit, the stuck-detector blocking a 4th identical call); truncation is only observable in the outgoing request and is covered by the extension tests. None has been counted across real sessions yet.
- **A batch cycle where a candidate beats the reference.** The one live cycle had a reference that already passed, so both editors chose `KEEP_BASE`; the fake-omp tests cover the other branches.
- **A merged team PR and a `shared` promotion from real branches.** Both are exercised on scratch repositories only.
- **Linux.** The code is POSIX-portable (`setsid`, process groups, `sh` scripts) and every run above was macOS arm64. No Windows.
- **Embeddings for the "related thing" lookup.** Exact signature and MinHash are implemented; embeddings would need credentials the binary does not hold.

Open questions from the spec that the build did not settle are at the end of [`docs/SPEC.md`](https://github.com/vncp/rematch/blob/main/docs/SPEC.md#open-questions).

## Claude Code: known limits

**Known limits** (the reasons, from DESIGN §18.1 "What Claude Code cannot do", follow): no `truncate` hooks (no hook rewrites earlier tool results); `retrieve` once per prompt, not before every request, and its units stay in the conversation; no per-turn event (turns are read from the transcript at `Stop`); `stuck` sees completed calls only and not subagents; no approval dialog and no `rematch` tool for the model; agent copies refresh at session start only; reflection needs omp; batch mode (`run`) is omp-only.

1. `truncate` hooks: no hook rewrites earlier tool results or the outgoing request. `PostToolUse` `updatedToolOutput` [2.1.121] replaces a result only as it arrives (age 0), while `truncate` shortens results once they are `max_age_turns` user turns old, so `truncate` entries are not run and not counted.
2. `retrieve` runs once per prompt, not before every model request, and the injected units stay in the conversation (omp re-sends one non-persisted message per request); a unit set equal to the last one is not injected again.
3. Skill loading is not per process: the shared dir serves every Claude Code session (and omp and Codex), holding the union of what every synced cwd matched, and a concurrent session sees links added after its scan only after its next re-scan. Its trace still lists, and counts, only what was matched for its own cwd.
4. No per-turn event: `turn_end` records are recovered at `Stop` from the transcript tail, which can lag; then one `turn_end` without model/usage carries `last_assistant_message`. Model fallbacks inside a run are seen only as turn-to-turn model changes.
5. Esc fires no hook: the abort is read from the transcript at the next prompt (`[Request interrupted by user…]`), and that prompt is not a takeover; a session that ends right after Esc closes the run at `SessionEnd`. A prompt queued while Claude works is a takeover only because `UserPromptSubmit` fires when it is delivered into the run (verified facts in DESIGN §18.1); if a release delivered it after `Stop`, it would be idle.
6. The `stuck` window sees completed calls only (parallel identical calls in flight are not counted); subagents get `block` rules but no `stuck` detection.
7. Hooks cannot open dialogs: the digest is a `systemMessage`/`additionalContext` at session start and approvals are commands the user runs (`/rematch:rematch-approve <id>`, or the shell); there is no one-tap approval, and no `rematch` tool for the model (Claude can run the CLI through Bash; the approve/decline/off/on/mark commands are not model-invocable). Reflection runs detached from the `SessionStart` hook and needs omp for its model calls; without omp it is `rematch reflect --pending` from a shell (or the next omp session start) after configuring the roles.
8. Agent copies are refreshed at `SessionStart` and `skills sync --harness claude-code` only, so a status change reaches Claude Code at the next start; omp-only frontmatter (`model` roles, unknown tools, other keys) is dropped from the copy.
9. A session killed before `SessionEnd` (or past its 1.5 s budget) keeps an unfinished trace that `count` skips — the same failure mode as an omp crash.
10. Imported Claude Code sessions carry no `loaded_entries` (transcripts do not record what the plugin linked).
11. Batch mode (`run`, anchor replay) drives omp rollouts only (`batch/worker.rs`, `Substrate::rollout_cmd` are omp-typed, the plan for another harness is §18.2.4).

## Codex: what works and what does not

| | Codex |
|---|---|
| Learning from sessions | **post-hoc**: `rematch import --history --harness codex` reads the rollouts, then `rematch reflect --history` reflects on the flagged ones (same redirect rules, rate limits, ≤ 3 model calls each, run from a shell) |
| Entries reaching Codex | **skills only**: the shared directory `~/.rematch/skills/` that omp and Claude Code load too (a link per kept entry and per provisional entry whose predicate matched a directory that was synced), seen by Codex through one symlink; `rematch skills sync --harness codex --cwd <repo>` adds the repo's matches |
| Counting | **post-hoc**: every Codex rollout records the skill list Codex gave the model; `import --history --harness codex` resolves the rematch skills in it (a link in the shared dir → its entry dir's `rematch.json`; hook routers are skipped, their hooks never run in Codex) as the session's loaded entries. Clean sessions get their trace written at import and `rematch count` (or the next `reflect`) folds them; flagged ones are counted when `reflect --history` handles them |

Limitations, all of them structural:

- **No live tracing.** rematch has no code inside Codex. A session becomes evidence only when you import its rollout afterwards; until then it is invisible to the counters.
- **No blocking hooks.** Compiled `block`/`stuck` verifiers (DESIGN §10) never run in Codex; an approved hook entry's `SKILL.md` router is in the shared directory and listed by Codex, but is never counted as loaded there. Codex's own hook mechanism is not wired.
- **No retrieval and no truncation.** The `retrieve` and `truncate` context policies need a pre-request handler; Codex sessions get neither.
- **No per-session gating.** Codex loads every skill under its skill roots in every session. The shared directory holds the union of what every synced directory matched (from Codex, omp or Claude Code); a provisional skill stays visible in every Codex session, in any repo, until it is kept (re-linked) or retired (removed). Counting still attributes it only where its predicate hits, so the union costs skill-list space, never a counter.
- **Loaded entries are reconstructed, not recorded.** They are resolved when you import, from the skill list in the rollout and the files as they are then: an entry created after the session started is not attributed (it is a newer version behind the same name), and a link you removed before importing is not seen.
- **No subagents, scripts run only after approval.** `kind: agent` entries have no Codex form and are not linked. Scripts inside a linked skill keep their mode: executable only after `rematch library approve <id>`.
- **No batch mode yet.** `rematch run` drives omp; the Codex rollout plan is in DESIGN §18.2.4.

Post-hoc reflection, post-hoc counting and skills are the whole adapter.

## How it is checked

The checks every change goes through:

```sh
cargo test --workspace                        # 119 core · 54 binary · 32 end-to-end (scripted editors, fake omp)
cargo clippy --workspace --all-targets -- -D warnings
bun build extension/index.ts --no-bundle --outdir /tmp/rematch-ext-check
bun test extension                            # hook runtime: mode helpers + the factory against a fake host
```

The end-to-end tests run the built binary against a temporary root: a synthetic trace with a redirect is reflected through a scripted editor, staged, matched, counted through four clean sessions and kept into the agent skills dir; the lint-fail script exercises exactly one retry; a wrong-model reply fails closed; `import --history` runs against anonymized goldens; `improve` refuses, splits, stages a sibling and refuses a pinned skill; the batch worker completes a `k=1` cycle with a fake omp and keeps the winner on the next run; `ship` creates the branch and commit; `export` refuses at work by default.

What was run live against omp 18.12.6 before this README was written: the full interactive loop on a scratch repository (rpc-mode sessions with real redirects, gemini-4-argon editor and classifier, the entry loaded in the next session and counted), the forge import and the mechanical split of a 31 KB skill resolved as a package in a live session, one detached batch cycle with a script verifier, one team ship, and `/rematch` in the TUI.

---

Source: README.md ("Status", "Run it"); adapters/claude-code/README.md ("Known limits"); docs/DESIGN.md §18.1 ("What Claude Code cannot do", quoted); adapters/codex/README.md ("What works and what does not").
