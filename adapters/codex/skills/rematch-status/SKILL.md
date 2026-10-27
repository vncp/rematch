---
name: rematch-status
description: Show what rematch staged, kept or retired, refresh the rematch skills Codex sees for this repo, and walk through pending rematch approvals. Use when the user types $rematch-status or asks about rematch, its digest, its entries or its approvals. Not for reflecting on sessions (the user runs `rematch reflect --history` from a shell).
---

Report rematch's state for the current repository through the `rematch` binary. Every command
below writes under `~/.rematch/`, outside the workspace, so each one needs the user's approval
in Codex; run them from the session's working directory and pass it as `--cwd`.

1. Refresh the skills Codex sees for this repo:
   `rematch skills sync --harness codex --cwd "$PWD" --text`
   Show the printed lines as they are. Newly linked skills reach Codex in the next session, not this one.
2. Read the digest: `rematch digest --cwd "$PWD"` prints one JSON line. Show every string in
   `data.lines` as it is, one per line.
3. For each item in `data.approvals` (`id`, `name`, `kind`, `reason`, `diff`, `commands`): show the
   name, the reason and the diff, then ask the user whether to approve it. Run the item's
   `commands.approve` (it ends with `--sha <hash>`, which binds the approval to what you showed)
   only after the user says yes to that item, and its `commands.decline` only after the user says
   to decline it. Never write the hash yourself; a refused hash means the item changed, so show
   `rematch library inspect <id> --text` again. Without an answer, leave it pending; it stays in
   every digest and in `rematch status`.
4. When the user asks why a session behaved as it did: `rematch library why codex:<session id> --text`
   (imported Codex sessions are named `codex:<rollout id>`; `rematch library ls --text` lists entries
   with their counters).

Do not edit files under `~/.rematch/`, `~/.agents/skills/` or rematch's staged skills yourself;
rematch writes them.
