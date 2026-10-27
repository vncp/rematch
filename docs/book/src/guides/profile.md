# Syncing machines with the profile

The profile is a git repo at `~/.rematch/profile/` that holds your omp setup: what rematch kept, the skills and agents you wrote by hand, approved hooks, `policy.json`, model roles per machine, branch `domain.yml` files and improver prompts you edited. Every machine that syncs it loads the same setup. (Not omp's `--profile`: this one follows you across machines.)

**First machine** — create a private, empty git repo anywhere (GitHub, a server, a USB disk), then:

```sh
rematch profile init --remote git@github.com:you/omp-profile.git --adopt --text
```

`--adopt` moves your kept entries and the hand-written skills in `~/.omp/agent/skills/` and agents in `~/.omp/agent/agents/` into the profile and leaves a link where each was, so omp loads them as before. Hand-written skills become **pinned** entries: measured like any entry, never moved, replaced or retired by the loop. Without `--remote` the profile stays local until you add one (`git -C ~/.rematch/profile remote add origin <url>`).

**Every other machine:**

```sh
rematch profile init --remote git@github.com:you/omp-profile.git --text
```

It clones, links every skill, agent and hook into `~/.omp/agent/` and `~/.rematch/skills/`, and pushes the machine's name into `profile.json`. Add `--adopt` to bring that machine's own skills in too.

**One more skill or agent:** `rematch profile adopt ~/.agents/skills/scratch` (a skill dir) or `rematch profile adopt ~/.omp/agent/agents/reviewer.md`.

**Syncing.** It happens by itself: each omp process runs `rematch profile sync` in the background at its first session start when a profile exists (bounded to 8 s; offline, the session simply goes on). A sync commits local changes as `<machine> <time>`, pulls with rebase, relinks, and pushes. What it pulls loads in your next session. By hand: `rematch profile sync --text` (`--no-push` to only pull; `--timeout <s>` for the network part, default 10). To sync on a timer too:

```sh
rematch profile install-launchd --interval 300 --text   # writes ~/Library/LaunchAgents/com.vncp.rematch.profile-sync.plist
launchctl load -w ~/Library/LaunchAgents/com.vncp.rematch.profile-sync.plist
```

It writes the plist and prints the `launchctl load` line; it never loads it for you. Its output goes to `~/.rematch/profile-sync.log`.

**Keeps go to the profile.** Once a profile exists, a kept entry is written to `profile/skills/<name>/` (agents: `profile/agents/<name>.md`) with a link from `~/.omp/agent/`, and an approved hook to `profile/hooks/`. A skill kept on another machine loads here after the next sync; it is counted only on the machine that kept it (counters live in each machine's ledger), and a hook approved on another machine runs here only for entries this machine knows (built-in hooks are approved per machine).

**Model roles per machine** in `profile/config/roles.yml`, read before `~/.omp/agent/config.yml`:

```yaml
editor:                       # rematch-editor; also classifier, solver, or any modelRoles key
  default: anthropic/claude-opus-5-5
  Vincent-M5M.local: openai/gpt-5     # this hostname only
classifier: fakeprov/small-model      # every machine
```

`rematch doctor --text` shows `role rematch-editor: … (via profile:rematch-editor)`; `rematch profile status --text` lists this machine's roles.

**Conflicts.** When two machines changed the same file, the sync stops the rebase, leaves the repo clean at your local commit, does not push, and says which files; the digest shows `profile: 1 conflict(s) not synced (skills/hand-notes/SKILL.md); resolve with git -C ~/.rematch/profile pull --rebase, then rematch profile sync` until it is resolved. Resolve it like any rebase (`git -C ~/.rematch/profile pull --rebase`, edit, `git add`, `git rebase --continue`), then `rematch profile sync`.

**What never syncs.** The ledger and its counters, the lake, traces, pending sessions, provisional and draft entries (`staging/`, `drafts/`), the archive and predecessors, jobs, credentials, omp's own `config.yml` (model choices only through `roles.yml`), and inside each skill its `rematch.json` and `EXPERIENCE.md` (written from this machine's ledger). History moves through the lake, not the profile.

**Status and leaving.** `rematch profile status --text` shows branch, remote, ahead/behind as of the last fetch, uncommitted changes and conflicts. `rematch uninstall --yes` keeps `~/.rematch/profile/` (it is your repo), removes the links into it, and puts your hand-written skills and agents back in `~/.omp/agent/` as plain files.

**Kept on other machines.** `library ls` lists skills in the profile that this machine's ledger does not hold:

```
kept (profile, from machine-a) skill  run-tests-before-commit [profile] not counted on this machine
```

`library why <session>` lists them as `kept (profile, from machine-a) <name>: linked into the shared skills dir, not counted on this machine[ · read in this session]`. `report` adds `profile-kept (not counted on this machine): N`. The machine is the one that last committed that skill (cached in `profile/.index.json` at each sync).

**omp only.** `rematch profile adopt` takes omp skills (a dir with `SKILL.md`) and agents (`.md`) only. It refuses Claude Code and Codex files — anything under `~/.claude` (`$CLAUDE_CONFIG_DIR`), `~/.codex` (`$CODEX_HOME`), `~/.agents`, any `.claude`/`.codex`/`.agents` dir, and `CLAUDE.md`, `AGENTS.md`, `settings.json` anywhere — with: `the profile holds omp skills and agents only …; CLAUDE.md, settings and other harnesses' files stay out. To adopt a skill for omp, copy it to ~/.omp/agent/skills/<name> first`. Claude Code and Codex still load every kept skill through `~/.rematch/skills`.

---

Source: moved from docs/MANUAL.md ("Profile: one setup on every machine"); docs/DESIGN.md §20.
