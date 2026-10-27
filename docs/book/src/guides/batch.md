# Batch mode with a verifier

For work that has a mechanical pass/fail (tests, a check script, a parity file, a receipt):

```
rematch task add <id> --repo <path> --prompt "…" --verifier "<cmd>" [--max-time 10m]
rematch run --task <id> --verifier "<cmd>" [--width 2]      # returns a job id, runs detached
rematch status <job-id>
```

The worker runs the task in a fresh git worktree with the current harness, proposes an invariant and a domain candidate, probes each, composes, re-runs, and lets the verifier pick. Four rollouts at the default width, ten at `--width 2`. The winner's lessons are staged **provisional**; they become kept only when the *next* run of that task passes with them loaded. The receipt is at `~/.rematch/runs/<task>/receipt.json`.

**Replacing a kept skill.** When a new version would replace a kept one (a sibling of it from `improve` or a per-family split, or a batch winner with the same name), rematch first re-runs the kept skill's own task once in a fresh worktree, with the new version loaded instead of the old. That task is the one the kept skill came from (or whose last run staged or loaded it), and it must have a verifier. If the verifier passes, the replacement goes ahead and the verdict is recorded in the new version's receipt. If it fails, the new version is retired with the note `anchor replay failed: …` and the kept skill stays. If there is no such task with a verifier, the new version's own clean sessions are the only check, as for any other entry. It is one run, not a test suite. To see what would happen without changing anything:

```
rematch anchor replay <id>     # runs the replay once and reports pass/fail; stages, keeps and retires nothing
```

## The cycle

Where a verifier exists, the plugin spends rollouts the way Harness² does, per task:

1. Reference rollout with the current harness, in a fresh git worktree with a `--config` overlay that exposes only the candidate skill tree.
2. Invariant and domain candidates from the single editor, each probed once on the same task under the same seed.
3. A composer merges from the behavioral contrasts; the composed harness runs once more.
4. The verifier picks among reference, candidates and composed: a test command, a deterministic check script, a parity check against a known result, or a receipt. Ties go to the reference.
5. The winner is kept for that job. Its lessons enter the library as `provisional` with a `pending_next_run` flag; they become `kept` only after the next real run of that job passes with them loaded.

Budget: `rollouts = (k+1)² + [k>1]`, four at `k=1`, ten at `--width 2`; model calls `2k + 1`. `run` returns a job id and runs detached; `status <job>` polls. Workspaces are deleted after the receipt is written.

## Tasks and verifiers

From DESIGN §17:

Task (`tasks/<id>.json`): `{id, repo (absolute), prompt, verifier?, max_time_secs (default 600), tools?, seeds (default [1]), created_at}`. omp has no sampling seed: the seed names the workspace and is exported to the rollout as `REMATCH_SEED`.

Verifier specs (`run --verifier`, else the task's): `tests` (Cargo.toml → `cargo test`; pyproject/pytest.ini/setup.* → `python3 -m pytest -q`; package.json + bun lock → `bun test`), `parity:<produced>=<expected file | sha256>`, `receipt:<path>` (JSON `verdict: pass|fail`, optional `score`), `[script:]<cmd>` (`sh -c` in the worktree, exit 0 = pass, last stdout JSON line with `score` supplies the score). Verifiers run with env `REMATCH_TASK`, `REMATCH_WORKTREE`, `REMATCH_REPO`, capped at max(task cap, 60 s).

Batch mode drives omp only: there is no Claude Code or Codex batch run (the Codex plan is DESIGN §18.2.4).

---

Source: moved from docs/MANUAL.md ("Batch mode with a verifier"); README.md ("Batch mode"); docs/DESIGN.md §17 (quoted), §18.1, §18.2.4.
