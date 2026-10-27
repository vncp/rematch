# Day to day

The short version: **use omp as usual.** rematch watches, proposes when you correct the agent, and keeps only what later sessions confirm. Everything below is optional.

Every `/rematch …` command in omp is the same as `rematch …` in a terminal; use whichever is in front of you. Add `--text` in the terminal for readable output instead of JSON.

1. Work in omp. Nothing to do.
2. When you tell the agent it got something wrong (*"no, …"*, *"that's wrong"*, *"revert that"*, interrupting it, or three tool errors in a row), the session is flagged at exit.
3. Next time you open omp, a reflection runs in the background (≈20 s, up to 3 model calls). If it produces something, you see one line at the top of the session:

   ```
   staged skill src-lib-public-functions in my-repo: Use when adding or modifying public functions in src/lib.py.
   ```

   That skill is now **provisional**: it loads in sessions whose repo and task look like the one that produced it, and nowhere else.
4. Keep working. Each later session where it loaded is counted: did the same kind of correction happen again (recurrence), did something break (harm)? After 4 clean sessions it is **kept** and copied into `~/.omp/agent/skills/`. After 2 recurrences or 2 harms it is **retired**. You are never asked.

Costs: nothing during a session; the reflection runs between sessions on the fast/strong roles you configured (`rematch doctor --text` shows which).

## Managing the library

```
rematch library ls --text
rematch library pin <id>        # human-owned: never replaced or retired automatically
rematch library unpin <id>
rematch library disable <id>    # retire now (archived with its evidence; never deleted)
rematch library rollback <name> # restore the previous kept version (skill or agent), retire the current one
rematch library why <session> --asof 2026-10-20T13:05   # what the session loaded, with each entry's status and each redirect's class as they stood then
```

Sharing beyond this machine: [Sharing to a team or fork](sharing.md).

## Turning it off, rolling back, uninstalling

| | |
|---|---|
| `/rematch off` · `/rematch on` | stop/resume tracing and reflection immediately (`~/.rematch/off` marker) |
| `REMATCH=0 omp` | off for one process |
| `/rematch disable <id>` | retire one entry now |
| `/rematch rollback <skill>` | previous version back, immediately |
| `rematch uninstall` | prints what it would remove; `--yes` removes every file rematch wrote (root, hooks, kept entries, restoring predecessors) and nothing else; a profile (`~/.rematch/profile/`, your git repo) stays, only the links into it go |

---

Source: moved from docs/MANUAL.md (intro, "Day to day", "Managing the library", "Turning it off, rolling back, uninstalling"); docs/DESIGN.md §2.
