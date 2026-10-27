# Predicates and domains

Every entry carries a deterministic trigger predicate. It decides where the entry loads (load-time match) and which finished sessions count for it (count-time match), computed the same way for every session (research D12: counters are meaningless unless "matched" is computed identically for every session). No model is involved at any point below.

## What a predicate holds

Repo path, the extensions of files touched in the evidence window, and up to ten stems ranked by tf-idf over the task turn, the touched files and the redirect. The hash of this predicate keys the rate limits. (README, "How a reflection works".)

The exact rules, from DESIGN §6:

Derivation (binary, on stage): `repo_glob` = the originating session's repo path (exact, as recorded by the extension from `git rev-parse --show-toplevel`) or `**` if no repo; `file_exts` = extensions of files read/edited in the evidence window (≤8); `stems` = up to 10 Porter-stemmed, stop-worded, lowercase tokens ranked by tf-idf against the ledger's session corpus over a weighted document: the originating task turn (the last user input before the redirect) ×2 + basenames of touched files ×2 + the redirect turn ×1. The redirect's own wording ("that's wrong", "every") must not crowd out the task's domain terms, or later sessions on the same task never count (observed on the first real run; fixed in `reflect.rs`). The editor's `entry.stems` are used only when derivation yields nothing: the pre-editor predicate hash is what the rate limits key on.

## When it matches

Place (ADR 001, `predicate::matches_place`): a predicate carrying `repo_remote`, at a place that has one too, matches when the remotes are equal and its `subpath` glob (`None`, empty or `**` = the whole repo) matches the toplevel-relative cwd or one of its ancestors; when either side has no remote the repo glob decides as below. So a worktree or another clone of the same remote matches its repo's entries wherever it lives, and a fork (another remote) does not. `repo_remote` = `identity::normalize_remote` of `origin`, else of the first remote: `host/org/repo`, scheme, user, port, `.git` and trailing `/` stripped, host lowercased, path lowercased on github.com and gitlab.com; a repo without a remote is `local:<sha256 of the main worktree's toplevel>` (linked worktrees share it). Derivation (`predicate::derive`) fills `repo_remote` from the repo on disk and `subpath` = `**`, keeping `repo_glob` as the fallback; `predicate_hash` leaves both out (they restate the glob's repo). The place of `match` is `Place::locate(repo, cwd)` (file reads), of a recorded session `Place::of_session` (its `repo_remote`/`subpath`).

Load-time match (`match --cwd`): the place matches (glob against repo, else cwd, when there is no remote) ⇒ branch dir contributed when ≥1 provisional non-agent, active entry in that branch matches. A `kind: hook` entry is active only while its approved module `rematch-hooks/<id>.ts` exists (`library::hook_active`): before approval it contributes no dir and is listed with `predicate_hit: false`, so it never counts. Provisional agents are listed in every session (omp discovers them from the plugin root regardless of the cwd) with `predicate_hit` = their glob hit, so the trace's `loaded_entries` carries them only where they would count. Both the given path and its `canonicalize()`d form are tried (macOS `/tmp` → `/private/tmp`), on either side.

Count-time match (binary, from a finished trace): place hit (above) AND (`file_exts` empty OR overlap ≥1) AND (`stems` empty OR overlap ≥ min(2, stems.len())). Inputs = first 5 `input` texts + all `tool_result.path` values. An entry whose version `session_start` recorded counts only at that version (`counters::fold_session` matches `(id, version)`; `retrieval` credit stays by id).

When an entry you expected did not load or count, `rematch match --cwd .` shows the load decision and `rematch library why <session>` shows `glob hit · predicate miss` per entry ([Troubleshooting](../guides/troubleshooting.md)).

## Domains

Every session and every trigger can carry a **domain**: a dotted lowercase path such as `web`, `web.frontend`, `mobile`, `quant.options` or `3d.animation`. When both an entry's trigger and a session have one, the entry loads and counts only if the session's domain equals the trigger's or extends it (`quant` covers `quant.options`; `web` never covers `mobile`). So a review skill learned on web work never triggers on mobile or 3D work, even when the words are the same. When either side has no domain, nothing changes: entries from before domains, and sessions rematch could not place, behave exactly as before.

How a session's domain is decided, first answer wins (no model is involved):

1. **You declared it in `~/.rematch/policy.json`.** Use the command, which is the only thing that writes that file (other keys in it are kept):

   ```sh
   rematch policy domain set github.com/acme/web web.frontend          # a normalized remote…
   rematch policy domain set . web.frontend                            # …or a repo path (its remote is used)
   rematch policy domain set 'github.com/acme/*' quant                 # a glob over remotes
   rematch policy domain set github.com/acme/app mobile --subpath ios  # only below ios/ in that repo
   rematch policy domain unset github.com/acme/app --subpath ios
   rematch policy show --text
   ```

   The file holds `"domains": [{"match": {"repo_remote": "...", "subpath": "..."}, "domain": "..."}]`; the command keeps the most specific declarations first (with a subpath, then an exact remote, then globs).
2. **The branch says so in `~/.rematch/branches/<branch>/domain.yml`** (`domain: quant.options`; the same file that lists the branch's facets, see [domain.yml.md](../reference/domain-yml.md)). The branch is the repo's directory name.
3. **Inference** from the files the session touched and the words it used, with the table in `~/.rematch/improver/domain_rules.toml` (yours to edit; an upgrade does not overwrite an edited copy). Extensions score 2 each, stems 1 each: `tsx jsx css html vue svelte` → `web`, `swift kt m dart` → `mobile`, `blend fbx gltf usd anim` → `3d`, `parquet sql ipynb csv` → `data`, `rs go c cpp java` → `backend`; words like backtest, sharpe, nbbo, option, strategy, portfolio → `quant`; anim, rig, bake, skeleton, keyframe → `3d.animation`; component, css, layout → `web.frontend`; ios, android, swiftui, compose → `mobile`. The best domain needs a score of at least 2 and must beat the runner-up by at least 1, otherwise the session gets none. A second level (`web.frontend`) only comes from a word rule.

What happens with it:

- **New entries take the domain of the session they came from** (reflection, `create --from task`); `create --from sessions` and `improve` take the domain most of their reference sessions share.
- **At session start** only rules 1 and 2 apply (the files and words are not known yet): `rematch match --cwd <dir>` prints the place's `domain`, and an entry of another domain does not load there. When the session ends, `count` decides the full domain and counts with it.
- **See it in `why`.** `rematch library why <session> --text` prints `domain <recorded> · policy now: <what today's policy decides> (policy.json|domain.yml|inferred)`, and for each loaded entry `domain <its domain> ok|mismatch` next to the place and predicate verdicts (a mismatch makes the place a miss):

  ```
  domain mobile · policy now: mobile (policy.json)
  loaded web-review v1 (provisional) sha 9d8209857232: domain web.frontend mismatch · place miss · predicate miss · this session not counted · …
  loaded ios-review v1 (provisional) sha 7947e0f201a9: domain mobile ok · place hit · predicate hit · this session recurrence 1 harm 0 · …
  ```
- **After declaring or changing a domain, run `rematch lake migrate`.** It re-decides every recorded session with the current policy and gives each entry its originating session's domain; a second run changes nothing. `policy domain set` tells you how many recorded sessions would change.
- **The digest proposes, never applies.** When at least five sessions in a repo without a declaration read skills that all belong to one domain, the session-start digest adds `declare domain <d> for <remote>: rematch policy domain set <remote> <d>`.
- **A cap per domain.** At most 8 kept entries per top-level domain across all branches on this machine (`rematch policy caps set kept_per_domain <n>`); past it, a new candidate is merged into an existing entry instead, like the per-branch cap.
- **`report` groups by domain** (see [Reading the report](../guides/report.md)).

## Predicate tuning

```sh
rematch tune --dry-run --text                    # signals and proposals for every entry; writes nothing
rematch tune --entry ledger-helper --text        # stage the tune of one entry (id or name)
rematch tune --window 30d --dry-run              # <n>w, <n>d or <n>h, ending now (default 6w)
```

A trigger predicate is a guess made at stage time. Sessions tell whether it was right: the model opens a skill (`skill://name`, `skills/<name>/SKILL.md`, a retrieved unit) the predicate did not match, or the predicate matches and the skill is loaded but never read. `tune` counts both per entry over the window and proposes a new predicate — no model call — that `count` then judges like any new entry.

- **Read-but-missed** (the stems missed): a session read the entry while it was visible (the shared skills dir exposed it, it was loaded, or it is your own skill registered in place) at a place the predicate covers, and the predicate did not hit. Three such sessions widen it: the stems found in at least two of them join the current ones (ranked by tf-idf, at most 10), and the file extensions they touched join a non-empty extension list (at most 8; an empty list already matches every extension).
- **Matched-but-unread** (the stems overmatch): the predicate hit, the entry was loaded, nothing read it. Four such sessions, with fewer than three read-but-missed, narrow it: the stems only unread sessions carry go, two always stay.
- **Domains.** A read in a domain the entry's domain does not cover (`cross_domain_reads`) or at a place the predicate does not cover (`cross_place_reads`) is shown and never used. An entry without a domain whose reads all carry one domain, in at least three sessions, gets that domain. A domain is never changed or widened, and stems are never pooled across domains.
- **Nothing is edited in place.** A tune stages a provisional sibling: the same files, version + 1, `sibling_of` the parent, a new predicate (a new canary, under the usual lint and rate limits) and a `status_note` such as `predicate tune: +[orderbook, parser, quot] -[] from 3 session(s)`. The session ids behind it are in the sibling's `rematch.json` (`tune.evidence`) and its proposal row. The parent keeps serving; the counters keep or retire the sibling, and a kept sibling replaces a kept parent through the anchor replay.
- **Guards.** At most one tune per entry every 7 days and one pending tune at a time; never on pinned, built-in or hook entries; never a change of repo, remote or subpath, never `**`; a provisional parent rematch staged is tuned once its own canary has kept it (its staging slot holds one version). `--dry-run` runs every guard and says `would stage` or `skipped: <reason>`.
- **It runs by itself.** `count` (and so `reflect --pending`) runs the same pass after its decisions whenever it counted a session; its lines start with `tune:`.
- **Reading it back.** `library why <session>` prints `predicate tune v2 pending (…)` under the parent while the sibling is provisional. `report` lists every tune in the window with both versions' with-arms side by side and the parent predicate's before-arm, and counts each session against the predicate of the version it loaded, so a wider predicate cannot improve a verdict by matching more sessions.

On a root where `ledger-helper` (stems `ledger helper round audit`) was read in three imported sessions about the order book parser:

```
tune · source ledger · window 6w (2026-09-15 00:00 UTC → 2026-10-27 00:00 UTC) · 3 session(s) · dry run
ledger-helper v1: read-but-missed 3 · matched-but-unread 0 · matched-and-read 0 → predicate tune: +[orderbook, parser, quot] -[] from 3 session(s) → would stage v2 (provisional, predicate b2c09424571f)
```

---

Source: docs/DESIGN.md §6 (derivation, load-time and count-time match, quoted); README.md ("How a reflection works"); moved from docs/MANUAL.md ("Domains", "Predicate tuning"); docs/research.md D12.
