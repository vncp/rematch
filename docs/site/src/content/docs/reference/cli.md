---
title: CLI
description: Every rematch subcommand with its flags, from the binary's own --help.
editUrl: false
tableOfContents:
  maxHeadingLevel: 2
---

:::note[Generated]
From `rematch 0.1.0` by `docs/site/scripts/gen.ts` (`bun run gen`). Do not edit by hand.
:::

Every subcommand prints one JSON envelope on stdout unless `--text` asks for readable lines (DESIGN §2):

| Outcome | stdout | Exit |
|---|---|---|
| success | `{"ok":true,"operation":…,"data":…}` | 0 |
| failure | `{"ok":false,…,"error":…}` | 1 |
| usage error | — | 2 |
| cancelled | — | 130 |

In omp, most `/rematch <command>` invocations run the same command; the exceptions are listed in [day to day](/rematch/guides/day-to-day/#omp-commands-and-the-cli).

## Commands

| Command | What it does |
|---|---|
| [`doctor`](#rematch-doctor) | omp binary + version, roles, extension link, ledger schema, roots, off state |
| [`import`](#rematch-import) | Import session history (omp, Claude Code, Codex, Pi, OpenCode) and/or a compound-forge checkout |
| [`reflect`](#rematch-reflect) | Count finished traces, then reflect on pending sessions (idempotent) |
| [`match`](#rematch-match) | Staging dirs, entries and hooks that load for a directory |
| [`skills`](#rematch-skills) | The shared skill directory every harness loads (REMATCH_ROOT/skills): sync |
| [`retrieve`](#rematch-retrieve) | Units of the loaded skill packages that match a query (the `retrieve` context hook) |
| [`check`](#rematch-check) | Detect and run the repo's fast check (Cargo, tsc, ruff, pyright, mypy, Go) plus unedited caller impact |
| [`digest`](#rematch-digest) | Session-start digest: new statuses, approvals, review list, transfers |
| [`record`](#rematch-record) | Append an event for a session (marks); the trace file stays the primary input |
| [`trace-event`](#rematch-trace-event) | Harness hook payload on stdin → DESIGN §3 trace records (Claude Code, OpenCode adapters) |
| [`hook-check`](#rematch-hook-check) | Harness pre-tool payload on stdin → approved block/stuck hooks verdict (Claude Code, OpenCode adapters) |
| [`create`](#rematch-create) | Greenfield: create an entry from sessions, a task, or a description (draft) |
| [`improve`](#rematch-improve) | Brownfield: register, audit, split or propose a sibling for an existing skill |
| [`activate`](#rematch-activate) | Draft → provisional |
| [`run`](#rematch-run) | Batch run with a verifier (detached; returns a job id) |
| [`status`](#rematch-status) | Job status (or all jobs, entry counts, pending actions and automatic trials) |
| [`library`](#rematch-library) | Library: ls, why, disable, rollback, pin, unpin, approve, decline, inspect, set-fork-path, export, import-export |
| [`task`](#rematch-task) | Batch task definitions |
| [`transfer`](#rematch-transfer) | Cross-branch transfer of kept invariant lessons |
| [`ship`](#rematch-ship) | Open a team/fork PR for entries |
| [`stale`](#rematch-stale) | Evidence drift: mark entries stale (or re-canary one) |
| [`split`](#rematch-split) | Per-family split of entries whose counters diverge |
| [`anchor`](#rematch-anchor) | Anchor replay of a kept entry's originating task with its sibling in place (manual: reports, stages nothing) |
| [`builtins`](#rematch-builtins) | Built-in entries (context unit retrieval, tool-output truncation, stuck detection) |
| [`count`](#rematch-count) | Fold finished traces into counters and decide |
| [`mark`](#rematch-mark) | Override the redirect class of a session's last turn |
| [`off`](#rematch-off) | Kill switch on (writes the `off` marker) |
| [`on`](#rematch-on) | Kill switch off (removes the `off` marker) |
| [`uninstall`](#rematch-uninstall) | Remove every file rematch wrote (plan only without `--yes`) |
| [`lake`](#rematch-lake) | The lake (ADR 001): export, import, rebuild, doctor, migrate |
| [`report`](#rematch-report) | The kill criterion (ADR 001 L4): kept entries in real use (≥ 4 clean actual uses), kept on comparative support, and recurrence per trigger, with-entry vs before-arm |
| [`policy`](#rematch-policy) | REMATCH_ROOT/policy.json: show, declare a repo's domain (domain set\|unset), caps (kept_per_domain), a skill's place scope (scope set\|unset) |
| [`tune`](#rematch-tune) | Predicate tuning: read-but-missed sessions widen a skill's stems, matched-but-unread narrow them; staged as a provisional sibling |
| [`profile`](#rematch-profile) | The profile (REMATCH_ROOT/profile, a git repo synced across machines): init, adopt, sync, status, install-launchd |

## rematch

```text
rematch: trace-grounded harness evolution for omp, Claude Code, Codex, Pi and OpenCode

Usage: rematch [OPTIONS] <COMMAND>

Commands:
  doctor       omp binary + version, roles, extension link, ledger schema, roots, off state
  import       Import session history (omp, Claude Code, Codex, Pi, OpenCode) and/or a compound-forge checkout
  reflect      Count finished traces, then reflect on pending sessions (idempotent)
  match        Staging dirs, entries and hooks that load for a directory
  skills       The shared skill directory every harness loads (REMATCH_ROOT/skills): sync
  retrieve     Units of the loaded skill packages that match a query (the `retrieve` context hook)
  check        Detect and run the repo's fast check (Cargo, tsc, ruff, pyright, mypy, Go) plus unedited caller impact
  digest       Session-start digest: new statuses, approvals, review list, transfers
  record       Append an event for a session (marks); the trace file stays the primary input
  trace-event  Harness hook payload on stdin → DESIGN §3 trace records (Claude Code, OpenCode adapters)
  hook-check   Harness pre-tool payload on stdin → approved block/stuck hooks verdict (Claude Code, OpenCode adapters)
  create       Greenfield: create an entry from sessions, a task, or a description (draft)
  improve      Brownfield: register, audit, split or propose a sibling for an existing skill
  activate     Draft → provisional
  run          Batch run with a verifier (detached; returns a job id)
  status       Job status (or all jobs, entry counts, pending actions and automatic trials)
  library      Library: ls, why, disable, rollback, pin, unpin, approve, decline, inspect, set-fork-path, export, import-export
  task         Batch task definitions
  transfer     Cross-branch transfer of kept invariant lessons
  ship         Open a team/fork PR for entries
  stale        Evidence drift: mark entries stale (or re-canary one)
  split        Per-family split of entries whose counters diverge
  anchor       Anchor replay of a kept entry's originating task with its sibling in place (manual: reports, stages nothing)
  builtins     Built-in entries (context unit retrieval, tool-output truncation, stuck detection)
  count        Fold finished traces into counters and decide
  mark         Override the redirect class of a session's last turn
  off          Kill switch on (writes the `off` marker)
  on           Kill switch off (removes the `off` marker)
  uninstall    Remove every file rematch wrote (plan only without --yes)
  lake         The lake (ADR 001): export, import, rebuild, doctor, migrate
  report       The kill criterion (ADR 001 L4): kept entries in real use (≥ 4 clean actual uses), kept on comparative support, and recurrence per trigger, with-entry vs before-arm
  policy       REMATCH_ROOT/policy.json: show, declare a repo's domain (domain set|unset), caps (kept_per_domain), a skill's place scope (scope set|unset)
  tune         Predicate tuning: read-but-missed sessions widen a skill's stems, matched-but-unread narrow them; staged as a provisional sibling
  profile      The profile (REMATCH_ROOT/profile, a git repo synced across machines): init, adopt, sync, status, install-launchd
  help         Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
  -V, --version    Print version
```

## rematch doctor

```text
omp binary + version, roles, extension link, ledger schema, roots, off state

Usage: rematch doctor [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch import

```text
Import session history (omp, Claude Code, Codex, Pi, OpenCode) and/or a compound-forge checkout

Usage: rematch import [OPTIONS]

Options:
      --history
          Import session history into the ledger

      --text
          Human-readable output instead of the JSON envelope

      --cwd <DIR>
          Directory the command runs against (repo/cwd for match, digest, create, …)

      --harness <HARNESS>
          Which harness's history: omp, claude-code, codex, pi, opencode, or all (default: each harness's default dir that exists)

          Possible values:
          - omp:         omp: `<agent dir>/sessions` plus every profile's sessions dir
          - claude-code: Claude Code: `~/.claude/projects` (`$CLAUDE_CONFIG_DIR/projects`)
          - codex:       Codex: `~/.codex/sessions` (`$CODEX_HOME/sessions`)
          - pi:          Pi: `~/.pi/agent/sessions` (`$PI_CODING_AGENT_SESSION_DIR`, `$PI_CODING_AGENT_DIR/sessions`)
          - opencode:    OpenCode: its session store `opencode.db` (`$OPENCODE_DB`, `$XDG_DATA_HOME/opencode`)
          - all:         Every harness whose default history dir exists
          
          [default: all]

      --sessions <DIR>
          Sessions directory of the selected harness (OpenCode: its `opencode.db` or the dir holding it; with `--harness all`: an omp sessions dir)

      --skip-profile <NAME>
          Skip a named profile during `--history` import (repeatable; merged with `policy.json` `skip_profiles`)

      --forge <PATH>
          compound-forge checkout: skills/* → provisional brownfield entries, learnings/*.md → lessons

      --migrate
          With --forge: copy each skill into rematch's own staging instead of registering it in place, so the forge checkout and plugin can be removed afterwards. Scripts are staged non-executable

  -h, --help
          Print help (see a summary with '-h')
```

## rematch reflect

```text
Count finished traces, then reflect on pending sessions (idempotent)

Usage: rematch reflect [OPTIONS]

Options:
      --pending             Process every pending reflection (the only mode; kept for the documented argv)
      --text                Human-readable output instead of the JSON envelope
      --cwd <DIR>           Directory the command runs against (repo/cwd for match, digest, create, …)
      --session <SESSION>   Only this session (count + reflect)
      --editor <EDITOR>     Scripted editor command replacing omp for every model call (tests)
      --detach              Run detached and return a job id
      --history             First hand flagged imported sessions (`import --history`) without a trace to reflection
      --since <YYYY-MM-DD>  With --history: only sessions started on or after this UTC date
      --limit <N>           With --history: at most this many sessions handed to reflection per run (default 10)
  -h, --help                Print help
```

## rematch match

```text
Staging dirs, entries and hooks that load for a directory

Usage: rematch match [OPTIONS]

Options:
      --inputs <INPUTS>  First inputs/paths of the session: a JSON array of texts, or `{"texts":[…],"paths":[…]}`. Refines `predicate_hit` with the count-time content rule
      --text             Human-readable output instead of the JSON envelope
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help             Print help
```

## rematch skills

```text
The shared skill directory every harness loads (REMATCH_ROOT/skills): sync

Usage: rematch skills [OPTIONS] <COMMAND>

Commands:
  sync  Link the skills that load for --cwd into REMATCH_ROOT/skills (adds matching provisional entries, keeps every kept/pinned one, prunes links of entries no longer loadable)
  help  Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch skills sync

```text
Link the skills that load for --cwd into REMATCH_ROOT/skills (adds matching provisional entries, keeps every kept/pinned one, prunes links of entries no longer loadable)

Usage: rematch skills sync [OPTIONS]

Options:
      --harness <HARNESS>
          The harness asking. `claude-code` also links the plugin's `skills/` and copies the agent entries into its `agents/` (plugin: $CLAUDE_PLUGIN_ROOT, else the `~/.claude/skills/rematch` install); `codex` reports whether Codex reaches the shared dir

          Possible values:
          - omp
          - claude-code
          - codex
          - pi:          Pi: the rematch Pi package returns the shared dir from `resources_discover`
          - opencode:    OpenCode: the shared sync only; the rematch OpenCode plugin delivers the dir

      --text
          Human-readable output instead of the JSON envelope

      --cwd <DIR>
          Directory the command runs against (repo/cwd for match, digest, create, …)

  -h, --help
          Print help (see a summary with '-h')
```

## rematch retrieve

```text
Units of the loaded skill packages that match a query (the `retrieve` context hook)

Usage: rematch retrieve [OPTIONS] --query <QUERY>

Options:
      --query <QUERY>              Request text: the last user message and recent tool-result paths
      --text                       Human-readable output instead of the JSON envelope
      --budget <BUDGET>            Byte budget for the returned unit texts [default: 6000]
      --cwd <DIR>                  Directory the command runs against (repo/cwd for match, digest, create, …)
      --min-overlap <MIN_OVERLAP>  Distinct query stems a unit must share to be returned [default: 2]
      --entries <ENTRIES>          Comma-separated entry ids the caller loaded; units of other entries are never returned
  -h, --help                       Print help
```

## rematch check

```text
Detect and run the repo's fast check (Cargo, tsc, ruff, pyright, mypy, Go) plus unedited caller impact

Usage: rematch check [OPTIONS]

Options:
      --files [<FILE>...]            Changed files to scope the check and impact analysis to
      --text                         Human-readable output instead of the JSON envelope
      --cwd <DIR>                    Directory the command runs against (repo/cwd for match, digest, create, …)
      --detect-only                  Detect the repo's check spec without running it
      --timeout <TIMEOUT>            Timeout in seconds (default 30) [default: 30]
      --budget-bytes <BUDGET_BYTES>  Byte budget for the rendered context message (default 4096) [default: 4096]
  -h, --help                         Print help
```

## rematch digest

```text
Session-start digest: new statuses, approvals, review list, transfers

Usage: rematch digest [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch record

```text
Append an event for a session (marks); the trace file stays the primary input

Usage: rematch record [OPTIONS] --session <SESSION> --event <EVENT>

Options:
      --session <SESSION>  
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --event <EVENT>      Event JSON (`{"ev":"mark","class":"correction"}`)
  -h, --help               Print help
```

## rematch trace-event

```text
Harness hook payload on stdin → DESIGN §3 trace records (Claude Code, OpenCode adapters)

Usage: rematch trace-event [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          Harness whose hook payload arrives on stdin

          Possible values:
          - claude-code
          - opencode:    The OpenCode plugin (`adapters/opencode`, `crate::opencode_event`)

      --text
          Human-readable output instead of the JSON envelope

      --cwd <DIR>
          Directory the command runs against (repo/cwd for match, digest, create, …)

      --session <SESSION>
          Session id (default: `cc:<payload session_id>`)

      --hook-output
          Speak the harness hook protocol: hook JSON on stdout, a block reason on stderr with exit 2; any error exits 0. Without it the result is the usual envelope

  -h, --help
          Print help (see a summary with '-h')
```

## rematch hook-check

```text
Harness pre-tool payload on stdin → approved block/stuck hooks verdict (Claude Code, OpenCode adapters)

Usage: rematch hook-check [OPTIONS] --harness <HARNESS>

Options:
      --harness <HARNESS>
          Harness whose hook payload arrives on stdin

          Possible values:
          - claude-code
          - opencode:    The OpenCode plugin (`adapters/opencode`, `crate::opencode_event`)

      --text
          Human-readable output instead of the JSON envelope

      --cwd <DIR>
          Directory the command runs against (repo/cwd for match, digest, create, …)

      --session <SESSION>
          Session id (default: `cc:<payload session_id>`)

      --hook-output
          Speak the harness hook protocol: hook JSON on stdout, a block reason on stderr with exit 2; any error exits 0. Without it the result is the usual envelope

  -h, --help
          Print help (see a summary with '-h')
```

## rematch create

```text
Greenfield: create an entry from sessions, a task, or a description (draft)

Usage: rematch create [OPTIONS] --from <FROM> <NAME>

Arguments:
  <NAME>  

Options:
      --from <FROM>                [possible values: sessions, task, description]
      --text                       Human-readable output instead of the JSON envelope
      --cwd <DIR>                  Directory the command runs against (repo/cwd for match, digest, create, …)
      --task <TASK>                
      --branch <BRANCH>            
      --description <DESCRIPTION>  
      --files <FILES>...           
      --ran <RAN>                  `--from sessions`: the references are sessions whose bash commands have every one of these words (comma-separated, case-folded; `--ran cargo,test` finds `cd x && cargo test --workspace`), ranked by their overlap with the request
      --editor <EDITOR>            Scripted editor command (tests)
  -h, --help                       Print help
```

## rematch improve

```text
Brownfield: register, audit, split or propose a sibling for an existing skill

Usage: rematch improve [OPTIONS] <SKILL>

Arguments:
  <SKILL>  

Options:
      --split-only       Only the mechanical split (no model, no session minimum)
      --text             Human-readable output instead of the JSON envelope
      --branch <BRANCH>  
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
      --editor <EDITOR>  Scripted editor command (tests)
  -h, --help             Print help
```

## rematch activate

```text
Draft → provisional

Usage: rematch activate [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --task <TASK>  Run the task once (reference rollout) and attach its trace as evidence
      --text         Human-readable output instead of the JSON envelope
      --cwd <DIR>    Directory the command runs against (repo/cwd for match, digest, create, …)
      --unverified   Accept the risk: activate without a trace
  -h, --help         Print help
```

## rematch run

```text
Batch run with a verifier (detached; returns a job id)

Usage: rematch run [OPTIONS] --task <TASK>

Options:
      --task <TASK>          
      --text                 Human-readable output instead of the JSON envelope
      --cwd <DIR>            Directory the command runs against (repo/cwd for match, digest, create, …)
      --verifier <VERIFIER>  Verifier spec (tests | parity:<produced>=<expected> | receipt:<path> | [script:]<cmd>); defaults to the task's verifier
      --width <WIDTH>        Candidates per editor scope (rollouts: (k+1)² + [k>1]) [default: 1]
      --seed <SEED>          Rollout seed (default: the task's first seed)
  -h, --help                 Print help
```

## rematch status

```text
Job status (or all jobs, entry counts, pending actions and automatic trials)

Usage: rematch status [OPTIONS] [JOB]

Arguments:
  [JOB]  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch library

```text
Library: ls, why, disable, rollback, pin, unpin, approve, decline, inspect, set-fork-path, export, import-export

Usage: rematch library [OPTIONS] <COMMAND>

Commands:
  ls             Every entry with raw counters
  why            Loaded entries, predicate hits, redirects and counters of a session (default: latest for --cwd)
  disable        Retire an entry now (user disable; counts as harm for that session). `id` is an entry id or the name of exactly one live entry
  rollback       Restore the predecessor of a kept skill or agent; retire the current one
  pin            
  unpin          
  approve        Approve a pending action exactly as reviewed: an entry id grants its scripts, hook and provisional-agent parts; `instructions:<id>` its personal text; `instructions-conflict:<name>` restores the approved text over your edit; `instructions-sync` rewrites the personal AGENTS.md; `ship-team:<id>`, `fork:<id>`, `probe:<id>`, `export:<id>` (`rematch status` lists them). `--sha` is required: the action's `review_sha256` (its approve command carries it)
  decline        Decline a pending action, bound by `--sha` like approve. An entry id retires the entry (its executable parts); `instructions:<id>` declines that text only, the entry stays; `instructions-applied:<name>` withdraws an applied text (profile-wide when the profile carries it); `instructions-conflict:<name>` keeps your edited text as your own; `fragment-unreadable:<id>` withdraws a runtime approval; ship, fork, probe and export items only stop the offer
  inspect        A pending action in full: target, reach, scope, exact diff and commands
  set-fork-path  Set the repo-relative file `ship --scope fork` writes for an entry (and, with --repo, the fork checkout it goes to)
  export         Work → home export of invariant entries
  import-export  Import an export directory
  help           Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library ls

```text
Every entry with raw counters

Usage: rematch library ls [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library why

```text
Loaded entries, predicate hits, redirects and counters of a session (default: latest for --cwd)

Usage: rematch library why [OPTIONS] [SESSION]

Arguments:
  [SESSION]  

Options:
      --asof <TS>  Also the status each loaded entry and the class each redirect had at this time (ms, RFC 3339, `YYYY-MM-DDTHH:MM[:SS]` UTC, or `YYYY-MM-DD` = 00:00 UTC), from the lake's `entries`/`redirects` or this ledger's journal
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library disable

```text
Retire an entry now (user disable; counts as harm for that session). `id` is an entry id or the name of exactly one live entry

Usage: rematch library disable [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --reason <REASON>  [default: "user request"]
      --text             Human-readable output instead of the JSON envelope
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help             Print help
```

### rematch library rollback

```text
Restore the predecessor of a kept skill or agent; retire the current one

Usage: rematch library rollback [OPTIONS] <SKILL>

Arguments:
  <SKILL>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library pin

```text
Usage: rematch library pin [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library unpin

```text
Usage: rematch library unpin [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library approve

```text
Approve a pending action exactly as reviewed: an entry id grants its scripts, hook and provisional-agent parts; `instructions:<id>` its personal text; `instructions-conflict:<name>` restores the approved text over your edit; `instructions-sync` rewrites the personal AGENTS.md; `ship-team:<id>`, `fork:<id>`, `probe:<id>`, `export:<id>` (`rematch status` lists them). `--sha` is required: the action's `review_sha256` (its approve command carries it)

Usage: rematch library approve [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --sha <SHA>  The full sha256 of the reviewed snapshot (`library inspect <id>` shows the command)
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library decline

```text
Decline a pending action, bound by `--sha` like approve. An entry id retires the entry (its executable parts); `instructions:<id>` declines that text only, the entry stays; `instructions-applied:<name>` withdraws an applied text (profile-wide when the profile carries it); `instructions-conflict:<name>` keeps your edited text as your own; `fragment-unreadable:<id>` withdraws a runtime approval; ship, fork, probe and export items only stop the offer

Usage: rematch library decline [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --sha <SHA>  
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library inspect

```text
A pending action in full: target, reach, scope, exact diff and commands

Usage: rematch library inspect [OPTIONS] <ID>

Arguments:
  <ID>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch library set-fork-path

```text
Set the repo-relative file `ship --scope fork` writes for an entry (and, with --repo, the fork checkout it goes to)

Usage: rematch library set-fork-path [OPTIONS] <ID> <PATH>

Arguments:
  <ID>    
  <PATH>  

Options:
      --repo <REPO>  
      --text         Human-readable output instead of the JSON envelope
      --cwd <DIR>    Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help         Print help
```

### rematch library export

```text
Work → home export of invariant entries

Usage: rematch library export [OPTIONS]

Options:
      --text             Human-readable output instead of the JSON envelope
      --to <TO>          Export dir (git-tracked; default REMATCH_ROOT/export)
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
      --entry <ENTRIES>  Entry id or name (repeatable); default: every kept invariant entry
      --approve          Approve the listed `--entry` exports (after seeing their diffs)
      --approve-all      Approve every candidate without per-entry review
  -h, --help             Print help
```

### rematch library import-export

```text
Import an export directory

Usage: rematch library import-export [OPTIONS]

Options:
      --from <FROM>  Export dir to read (default REMATCH_ROOT/export)
      --text         Human-readable output instead of the JSON envelope
      --cwd <DIR>    Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help         Print help
```

## rematch task

```text
Batch task definitions

Usage: rematch task [OPTIONS] <COMMAND>

Commands:
  add   Define (or redefine) a batch task
  ls    List task definitions
  help  Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch task add

```text
Define (or redefine) a batch task

Usage: rematch task add [OPTIONS] --repo <REPO> --prompt <PROMPT> <ID>

Arguments:
  <ID>  

Options:
      --repo <REPO>          
      --text                 Human-readable output instead of the JSON envelope
      --cwd <DIR>            Directory the command runs against (repo/cwd for match, digest, create, …)
      --prompt <PROMPT>      
      --verifier <VERIFIER>  
      --max-time <MAX_TIME>  Per-rollout time cap: `600`, `90s`, `10m`, `1h`
      --tools <TOOLS>        omp `--tools` list for rollouts
      --seed <SEEDS>         Rollout seed (repeatable); exported to rollouts and verifiers as `REMATCH_SEED`
      --editable <EDITABLE>  Repo-relative path a rollout may change (repeatable): the solution scope. Without one, runs still execute but mint no comparative evidence
  -h, --help                 Print help
```

### rematch task ls

```text
List task definitions

Usage: rematch task ls [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch transfer

```text
Cross-branch transfer of kept invariant lessons

Usage: rematch transfer [OPTIONS] <COMMAND>

Commands:
  suggest  List kept invariant lessons not yet tried in a structurally overlapping branch
  apply    Stage a provisional copy of a lesson in another branch
  promote  Copy entries kept in two or more branches to `shared` (kept, no new canary)
  help     Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch transfer suggest

```text
List kept invariant lessons not yet tried in a structurally overlapping branch

Usage: rematch transfer suggest [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch transfer apply

```text
Stage a provisional copy of a lesson in another branch

Usage: rematch transfer apply [OPTIONS] --branch <BRANCH> <LESSON_ID>

Arguments:
  <LESSON_ID>  

Options:
      --branch <BRANCH>  
      --text             Human-readable output instead of the JSON envelope
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help             Print help
```

### rematch transfer promote

```text
Copy entries kept in two or more branches to `shared` (kept, no new canary)

Usage: rematch transfer promote [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch ship

```text
Open a team/fork PR for entries

Usage: rematch ship [OPTIONS] --scope <SCOPE>

Options:
      --scope <SCOPE>    team (`<repo>/.omp/skills/`, `.omp/agents/`, `AGENTS.md`) or fork (`<repo>/<fork_path>`) [possible values: team, fork]
      --text             Human-readable output instead of the JSON envelope
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
      --repo <REPO>      Target repo (team: defaults to the entry's originating repo; fork: defaults to the entry's `fork_repo`)
      --entry <ENTRIES>  Entry id or name (repeatable). Listing an entry makes it eligible for team scope
      --push             Push the branch to `--remote` (default: no network)
      --pr               Open a PR with `gh pr create --fill --body-file` (requires --push; never merges)
      --remote <REMOTE>  [default: origin]
  -h, --help             Print help
```

## rematch stale

```text
Evidence drift: mark entries stale (or re-canary one)

Usage: rematch stale [OPTIONS]

Options:
      --dry-run           Report drift without moving entries to `stale`
      --text              Human-readable output instead of the JSON envelope
      --cwd <DIR>         Directory the command runs against (repo/cwd for match, digest, create, …)
      --recanary <ENTRY>  Bring a stale entry back as a new provisional version (new canary)
  -h, --help              Print help
```

## rematch split

```text
Per-family split of entries whose counters diverge

Usage: rematch split [OPTIONS]

Options:
      --review-days <REVIEW_DAYS>  Capability-floor window in days [default: 7]
      --text                       Human-readable output instead of the JSON envelope
      --cwd <DIR>                  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help                       Print help
```

## rematch anchor

```text
Anchor replay of a kept entry's originating task with its sibling in place (manual: reports, stages nothing)

Usage: rematch anchor [OPTIONS] <COMMAND>

Commands:
  replay  Replay the originating task of the kept entry <entry> would replace, with <entry> in its place; reports the verdict and changes no status
  help    Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch anchor replay

```text
Replay the originating task of the kept entry <entry> would replace, with <entry> in its place; reports the verdict and changes no status

Usage: rematch anchor replay [OPTIONS] <ENTRY>

Arguments:
  <ENTRY>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch builtins

```text
Built-in entries (context unit retrieval, tool-output truncation, stuck detection)

Usage: rematch builtins [OPTIONS] <COMMAND>

Commands:
  ls         The shipped built-in entries with their current entry, status, approval and counters
  reinstall  Stage a fresh provisional version of built-ins that are not live (retired, dormant, stale or missing); live ones are left alone. Default: every built-in
  help       Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch builtins ls

```text
The shipped built-in entries with their current entry, status, approval and counters

Usage: rematch builtins ls [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch builtins reinstall

```text
Stage a fresh provisional version of built-ins that are not live (retired, dormant, stale or missing); live ones are left alone. Default: every built-in

Usage: rematch builtins reinstall [OPTIONS] [NAMES]...

Arguments:
  [NAMES]...  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch count

```text
Fold finished traces into counters and decide

Usage: rematch count [OPTIONS]

Options:
      --session <SESSION>  
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --editor <EDITOR>    Scripted command for the classifier/reflector calls (tests)
  -h, --help               Print help
```

## rematch mark

```text
Override the redirect class of a session's last turn

Usage: rematch mark [OPTIONS] --session <SESSION> --class <CLASS>

Options:
      --session <SESSION>  
      --text               Human-readable output instead of the JSON envelope
      --class <CLASS>      [possible values: correction, preference, scope]
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help               Print help
```

## rematch off

```text
Kill switch on (writes the `off` marker)

Usage: rematch off [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch on

```text
Kill switch off (removes the `off` marker)

Usage: rematch on [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch uninstall

```text
Remove every file rematch wrote (plan only without --yes)

Usage: rematch uninstall [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --yes        Apply the plan (otherwise print it)
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch lake

```text
The lake (ADR 001): export, import, rebuild, doctor, migrate

Usage: rematch lake [OPTIONS] <COMMAND>

Commands:
  export   Journal rows past this machine's watermark and closed sessions' traces → Parquet
  import   Merge another machine's lake (absent partition files, blobs, watermarks)
  rebuild  A fresh ledger from the lake's journal partitions
  doctor   Watermarks, export lag, files per table, orphan files, the pre-journal snapshot range, the Bellwether pin and each table's index, sessions counted with no counters row
  index    Bellwether indexes per table under REMATCH_ROOT/lake-index/ with the ADR 001 key table (on demand; never synced)
  query    One key lookup over a lake table → rows (JSON): planned by the table's index and its residual, else a full scan (`indexed: false`)
  migrate  Fill identity fields on rows that predate them (content_sha256 on entries, repo_remote/subpath on predicates and sessions whose repo is on this machine) and (re)apply the domain policy to sessions and entry predicates (idempotent)
  help     Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch lake export

```text
Journal rows past this machine's watermark and closed sessions' traces → Parquet

Usage: rematch lake export [OPTIONS]

Options:
      --all        Drain the whole journal and every closed session in one run (default: one bounded run)
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch lake import

```text
Merge another machine's lake (absent partition files, blobs, watermarks)

Usage: rematch lake import [OPTIONS] --from <LAKE>

Options:
      --from <LAKE>  The other lake: its directory, or the REMATCH_ROOT that holds it
      --text         Human-readable output instead of the JSON envelope
      --cwd <DIR>    Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help         Print help
```

### rematch lake rebuild

```text
A fresh ledger from the lake's journal partitions

Usage: rematch lake rebuild [OPTIONS]

Options:
      --from <LAKE>        Lake to rebuild from (default: REMATCH_ROOT/lake)
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --force              Replace a non-empty ledger (the current file is kept as `ledger.sqlite.pre-rebuild-<ms>`)
      --machine <MACHINE>  Machine whose journal to replay (default: this machine's id)
  -h, --help               Print help
```

### rematch lake doctor

```text
Watermarks, export lag, files per table, orphan files, the pre-journal snapshot range, the Bellwether pin and each table's index, sessions counted with no counters row

Usage: rematch lake doctor [OPTIONS]

Options:
      --bellwether <PATH>  The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH)
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help               Print help
```

### rematch lake index

```text
Bellwether indexes per table under REMATCH_ROOT/lake-index/ with the ADR 001 key table (on demand; never synced)

Usage: rematch lake index [OPTIONS] [TABLES]...

Arguments:
  [TABLES]...  Tables to index (default: all): events sessions redirects entries counters proposals receipts outcomes

Options:
      --bellwether <PATH>  The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH)
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --repin              Rebuild every index under this binary's version when the lake pins another one
  -h, --help               Print help
```

### rematch lake query

```text
One key lookup over a lake table → rows (JSON): planned by the table's index and its residual, else a full scan (`indexed: false`)

Usage: rematch lake query [OPTIONS] --key <KEY> <TABLE>

Arguments:
  <TABLE>  The table: events sessions redirects entries counters proposals receipts outcomes

Options:
      --key <KEY>          A key of the table (ADR 001 key table), e.g. exact:path, text:tool_cmd, list:facets, temporal2:entry_id
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --values <VALUES>    Key values, comma-separated (list keys: elements; a two-column key: `a:b`, split at the last ':')
      --has <HAS>          Text keys (text:tool_cmd): words every row must have, comma-separated, case-folded (`--has cargo,test` finds `cd x && cargo test --workspace`)
      --from <FROM>        Window start (inclusive) on the table's time column: ms UTC, RFC 3339, or YYYY-MM-DD
      --to <TO>            Window end (inclusive), as --from
      --valid <VALID>      The version valid at this instant (temporal2; a temporal key's --at)
      --known <KNOWN>      As the ledger knew it at this instant (temporal2)
      --latest             The newest version per key (with --known: the newest known by then)
      --all                List keys: rows holding every value (default: any)
      --limit <LIMIT>      At most this many rows
      --bellwether <PATH>  The Bellwether binary (default: $REMATCH_BELLWETHER, else `bellwether` on PATH)
  -h, --help               Print help
```

### rematch lake migrate

```text
Fill identity fields on rows that predate them (content_sha256 on entries, repo_remote/subpath on predicates and sessions whose repo is on this machine) and (re)apply the domain policy to sessions and entry predicates (idempotent)

Usage: rematch lake migrate [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch report

```text
The kill criterion (ADR 001 L4): kept entries in real use (≥ 4 clean actual uses), kept on comparative support, and recurrence per trigger, with-entry vs before-arm

Usage: rematch report [OPTIONS]

Options:
      --text             Human-readable output instead of the JSON envelope
      --window <WINDOW>  Window ending now: `<n>w`, `<n>d` or `<n>h` [default: 6w]
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
      --since <DAY>      Start the window at this UTC day (`YYYY-MM-DD`) instead of `now − window`
  -h, --help             Print help
```

## rematch policy

```text
REMATCH_ROOT/policy.json: show, declare a repo's domain (domain set|unset), caps (kept_per_domain), a skill's place scope (scope set|unset)

Usage: rematch policy [OPTIONS] <COMMAND>

Commands:
  show    The policy file, the declared domains, caps and skill scopes in effect, and the branch domain.yml domains
  domain  Declare (set) or remove (unset) the domain of a repo remote (or remote glob, or a repo path)
  caps    Caps (`kept_per_domain`: kept entries per top-level domain across branches, default 8)
  scope   Place scope of a skill name: exposed only in one repo (by remote, any clone or worktree) or directory, never globally (set|unset; reserved to the user)
  probes  Automatic probe budget (`daily_runs`: probes per 24 h, default 3, 0 = off; `max_parallel`: default 1)
  auto    Standing automatic approval rules (`surface`: scripts|agents|instructions, `scope`: `<repo path | remote | **>`). Reserved to the user
  help    Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch policy show

```text
The policy file, the declared domains, caps and skill scopes in effect, and the branch domain.yml domains

Usage: rematch policy show [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch policy domain

```text
Declare (set) or remove (unset) the domain of a repo remote (or remote glob, or a repo path)

Usage: rematch policy domain [OPTIONS] <COMMAND>

Commands:
  set    Declare `<domain>` for sessions in `<target>` (a normalized remote, a glob over remotes, or a repo path on this machine)
  unset  Remove the declaration for `<target>` (and `--subpath`)
  help   Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy domain set

```text
Declare `<domain>` for sessions in `<target>` (a normalized remote, a glob over remotes, or a repo path on this machine)

Usage: rematch policy domain set [OPTIONS] <TARGET> <DOMAIN>

Arguments:
  <TARGET>  
  <DOMAIN>  

Options:
      --subpath <SUBPATH>  Only below this toplevel-relative path glob
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help               Print help
```

#### rematch policy domain unset

```text
Remove the declaration for `<target>` (and `--subpath`)

Usage: rematch policy domain unset [OPTIONS] <TARGET>

Arguments:
  <TARGET>  

Options:
      --subpath <SUBPATH>  
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help               Print help
```

### rematch policy caps

```text
Caps (`kept_per_domain`: kept entries per top-level domain across branches, default 8)

Usage: rematch policy caps [OPTIONS] <COMMAND>

Commands:
  set   Set a cap
  help  Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy caps set

```text
Set a cap

Usage: rematch policy caps set [OPTIONS] <CAP> <VALUE>

Arguments:
  <CAP>    [possible values: kept_per_domain]
  <VALUE>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch policy scope

```text
Place scope of a skill name: exposed only in one repo (by remote, any clone or worktree) or directory, never globally (set|unset; reserved to the user)

Usage: rematch policy scope [OPTIONS] <COMMAND>

Commands:
  set    Expose skill `<name>` only at `<target>`: a repo path on this machine (stored as its normalized remote, so every clone and worktree matches), a normalized remote (`github.com/org/repo`), or a directory outside git (stored as its path). Withdraws the name from REMATCH_ROOT/skills and <agent_dir>/skills, rewrites the place of its live entries and resyncs
  unset  Remove the scope of `<name>`: its kept and profile copies are global again (entry predicates keep the scoped place)
  help   Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy scope set

```text
Expose skill `<name>` only at `<target>`: a repo path on this machine (stored as its normalized remote, so every clone and worktree matches), a normalized remote (`github.com/org/repo`), or a directory outside git (stored as its path). Withdraws the name from REMATCH_ROOT/skills and <agent_dir>/skills, rewrites the place of its live entries and resyncs

Usage: rematch policy scope set [OPTIONS] <NAME> <TARGET>

Arguments:
  <NAME>    
  <TARGET>  

Options:
      --subpath <SUBPATH>  Only below this toplevel-relative path glob (default `**`, the whole repo)
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help               Print help
```

#### rematch policy scope unset

```text
Remove the scope of `<name>`: its kept and profile copies are global again (entry predicates keep the scoped place)

Usage: rematch policy scope unset [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch policy probes

```text
Automatic probe budget (`daily_runs`: probes per 24 h, default 3, 0 = off; `max_parallel`: default 1)

Usage: rematch policy probes [OPTIONS]

Options:
      --daily <DAILY>                Probes started per 24 hours (`0` turns automatic probes off)
      --text                         Human-readable output instead of the JSON envelope
      --cwd <DIR>                    Directory the command runs against (repo/cwd for match, digest, create, …)
      --max-parallel <MAX_PARALLEL>  Concurrent probe runs (default 1)
  -h, --help                         Print help
```

### rematch policy auto

```text
Standing automatic approval rules (`surface`: scripts|agents|instructions, `scope`: `<repo path | remote | **>`). Reserved to the user

Usage: rematch policy auto [OPTIONS] <COMMAND>

Commands:
  add     Add a standing rule auto-approving `<surface>` in `<scope>` (`**`, a normalized remote, or a repo path)
  remove  Remove a standing rule for `<surface>` in `<scope>`
  list    List the standing automatic approval rules
  help    Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy auto add

```text
Add a standing rule auto-approving `<surface>` in `<scope>` (`**`, a normalized remote, or a repo path)

Usage: rematch policy auto add [OPTIONS] <SURFACE> <SCOPE>

Arguments:
  <SURFACE>  [possible values: scripts, agents, instructions]
  <SCOPE>    

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy auto remove

```text
Remove a standing rule for `<surface>` in `<scope>`

Usage: rematch policy auto remove [OPTIONS] <SURFACE> <SCOPE>

Arguments:
  <SURFACE>  [possible values: scripts, agents, instructions]
  <SCOPE>    

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

#### rematch policy auto list

```text
List the standing automatic approval rules

Usage: rematch policy auto list [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

## rematch tune

```text
Predicate tuning: read-but-missed sessions widen a skill's stems, matched-but-unread narrow them; staged as a provisional sibling

Usage: rematch tune [OPTIONS]

Options:
      --entry <ENTRY>    Only this entry (id or name)
      --text             Human-readable output instead of the JSON envelope
      --cwd <DIR>        Directory the command runs against (repo/cwd for match, digest, create, …)
      --dry-run          Report signals and proposals (and whether each would stage); write nothing
      --window <WINDOW>  Window ending now: `<n>w`, `<n>d` or `<n>h` [default: 6w]
  -h, --help             Print help
```

## rematch profile

```text
The profile (REMATCH_ROOT/profile, a git repo synced across machines): init, adopt, sync, status, install-launchd

Usage: rematch profile [OPTIONS] <COMMAND>

Commands:
  init             Create REMATCH_ROOT/profile (git init, or clone --remote), then sync it
  adopt            Move one skill dir or agent .md file into the profile, a link left in its place
  sync             Commit local changes, pull --rebase, relink the views, push
  status           Branch, remote, ahead/behind (as of the last fetch), dirty paths, conflicts, machines
  install-launchd  Write ~/Library/LaunchAgents/com.vncp.rematch.profile-sync.plist (not loaded)
  help             Print this message or the help of the given subcommand(s)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch profile init

```text
Create REMATCH_ROOT/profile (git init, or clone --remote), then sync it

Usage: rematch profile init [OPTIONS]

Options:
      --remote <REMOTE>    Git remote to clone (an existing profile) or to push to (an empty one)
      --text               Human-readable output instead of the JSON envelope
      --adopt              Move kept entries, hand-written `<agent_dir>/skills/*` and `<agent_dir>/agents/*.md` into the profile (hand-written skills become pinned entries)
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --timeout <TIMEOUT>  Seconds for all network operations together [default: 10]
  -h, --help               Print help
```

### rematch profile adopt

```text
Move one skill dir or agent .md file into the profile, a link left in its place

Usage: rematch profile adopt [OPTIONS] <PATH>

Arguments:
  <PATH>  A skill directory (holding SKILL.md) or an agent file (`<name>.md`)

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch profile sync

```text
Commit local changes, pull --rebase, relink the views, push

Usage: rematch profile sync [OPTIONS]

Options:
      --no-push            Commit and pull, but do not push
      --text               Human-readable output instead of the JSON envelope
      --cwd <DIR>          Directory the command runs against (repo/cwd for match, digest, create, …)
      --timeout <TIMEOUT>  Seconds for all network operations together (fetch + push); past it the sync reports `offline: true` and goes on with the local state [default: 10]
  -h, --help               Print help
```

### rematch profile status

```text
Branch, remote, ahead/behind (as of the last fetch), dirty paths, conflicts, machines

Usage: rematch profile status [OPTIONS]

Options:
      --text       Human-readable output instead of the JSON envelope
      --cwd <DIR>  Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help       Print help
```

### rematch profile install-launchd

```text
Write ~/Library/LaunchAgents/com.vncp.rematch.profile-sync.plist (not loaded)

Usage: rematch profile install-launchd [OPTIONS]

Options:
      --interval <INTERVAL>  Seconds between syncs [default: 300]
      --text                 Human-readable output instead of the JSON envelope
      --cwd <DIR>            Directory the command runs against (repo/cwd for match, digest, create, …)
  -h, --help                 Print help
```
