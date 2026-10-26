# rematch for Codex

The same `rematch` binary, ledger, lint, counters and entries as the omp plugin, reached from Codex
(CLI, IDE extension, Codex in the ChatGPT desktop app) through two things Codex already has: its
session rollouts (`~/.codex/sessions/**/rollout-*.jsonl`) and its skill directories
(`~/.agents/skills`, `<repo>/.agents/skills`). Nothing here runs inside a Codex session, makes a model
call, or adds prompt text of its own. Design and verified format facts: [`docs/DESIGN.md`](../../docs/DESIGN.md) §18.2.

## What works and what does not

| | Codex |
|---|---|
| Learning from sessions | **post-hoc**: `rematch import --history --harness codex` reads the rollouts, then `rematch reflect --history` reflects on the flagged ones (same redirect rules, rate limits, ≤ 3 model calls each, run from a shell) |
| Entries reaching Codex | **skills only**: `rematch library sync --harness codex --cwd <repo>` links the provisional skills that match the repo into `~/.rematch/codex-skills/`, which Codex sees through one symlink; kept skills are copied to `~/.agents/skills/<name>/` |
| Counting | **post-hoc**: every Codex rollout records the skill list Codex gave the model; `import --history --harness codex` resolves the rematch skills in it (a synced link → its staging dir's `rematch.json`, a kept copy → its marker) as the session's loaded entries. Clean sessions get their trace written at import and `rematch count` (or the next `reflect`) folds them; flagged ones are counted when `reflect --history` handles them |

Limitations, all of them structural:

- **No live tracing.** rematch has no code inside Codex. A session becomes evidence only when you import its rollout afterwards; until then it is invisible to the counters.
- **No blocking hooks.** Compiled `block`/`stuck` verifiers (DESIGN §10) never run in Codex; hook entries are not synced. Codex's own hook mechanism is not wired.
- **No retrieval and no truncation.** The `retrieve` and `truncate` context policies need a pre-request handler; Codex sessions get neither.
- **No per-session gating.** Codex loads every skill under its skill roots in every session. `library sync` decides *which* provisional skills are in `~/.rematch/codex-skills/` (those whose predicate matches the repo you synced for); they stay visible in every Codex session, in any repo, until the next sync. Counting still attributes them only where their predicate hits.
- **Loaded entries are reconstructed, not recorded.** They are resolved when you import, from the skill list in the rollout and the files as they are then: an entry created after the session started is not attributed (it is a newer version behind the same name), and a link you removed before importing is not seen.
- **No subagents, scripts run only after approval.** `kind: agent` entries have no Codex form and are not synced. Scripts inside a synced skill keep their mode: executable only after `rematch library approve <id>`.
- **No batch mode yet.** `rematch run` drives omp; the Codex rollout plan is in DESIGN §18.2.4.

Post-hoc reflection, post-hoc counting and skills are the whole adapter.

## Install

Prerequisites: `rematch` on `PATH` (from this repo: `scripts/install.sh`, or `cargo build --release` and put `target/release/rematch` on `PATH`). The omp plugin is not required.

1. **Import your Codex history and reflect on it** (reads `~/.codex/sessions`, or `$CODEX_HOME/sessions`; read-only):

   ```sh
   rematch import --history --harness codex
   rematch reflect --history --since 2026-10-01      # editor/classifier models from the omp config (rematch doctor --text)
   ```

2. **Let Codex see rematch's provisional skills** (one symlink, once):

   ```sh
   mkdir -p ~/.agents/skills
   ln -s ~/.rematch/codex-skills ~/.agents/skills/rematch
   ```

   Codex scans `~/.agents/skills` recursively and follows directory symlinks, so every
   `~/.rematch/codex-skills/<name>/SKILL.md` appears as a skill. For one repo only, link it from
   that repo instead: `ln -s ~/.rematch/codex-skills <repo>/.agents/skills/rematch` (and add the
   link to the repo's `.git/info/exclude`).

3. **Sync before you start Codex in a repo**:

   ```sh
   rematch library sync --harness codex --cwd "$PWD"
   ```

   It rewrites the links for that repo, copies kept skills to `~/.agents/skills/<name>/`
   (marked with `.rematch-codex.json`; a directory of yours with the same name is reported as a
   conflict and never touched), and removes copies of entries that are no longer kept. To make it
   automatic, a shell function in your shell rc does it on every launch:

   ```sh
   codex() { rematch library sync --harness codex --cwd "$PWD" >/dev/null; command codex "$@"; }
   ```

4. **Optional: the `$rematch-status` skill** (digest, sync and approvals from inside Codex):

   ```sh
   ln -s "$PWD/adapters/codex/skills/rematch-status" ~/.agents/skills/rematch-status
   ```

   It runs `rematch` commands, which write under `~/.rematch/`, outside Codex's workspace sandbox:
   Codex asks you to approve each one.

5. **Optional config** (`config.toml` in this directory): how to switch one rematch skill off in
   Codex without touching rematch, and the skill-list budget.

After a few Codex sessions: `rematch import --history --harness codex && rematch reflect --history`.

## Marking a redirect

Codex has no `/rematch` command. Either write the mark as a message in the Codex session
(`/rematch mark correction`; the importer reads it from the rollout like the extension would, if your
Codex front end sends the text instead of rejecting the unknown slash command) or, after the session:

```sh
rematch library why codex:<session id>        # ids of imported Codex sessions are codex:<rollout id>
rematch mark --session codex:<session id> --class correction
```

## Uninstall

```sh
rematch uninstall --yes                                        # removes ~/.rematch, the kept copies under ~/.agents/skills (marked .rematch-codex.json) and the omp side
rm ~/.agents/skills/rematch ~/.agents/skills/rematch-status   # the links you made
```

Without `--yes`, `rematch uninstall` prints the plan, Codex copies included.

## Files

| File | Purpose |
|---|---|
| `README.md` | this page |
| `config.toml` | optional `~/.codex/config.toml` keys (commented) |
| `skills/rematch-status/SKILL.md` | `$rematch-status`: digest, sync, one-tap approvals through `rematch` |
