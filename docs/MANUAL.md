# rematch manual

The manual is the documentation site, **<https://vncp.github.io/rematch/>**, built from [`docs/book/`](book/src/SUMMARY.md) (`mdbook build docs/book`). Its guides hold everything this file used to; below is a quick reference.

**Use omp as usual.** rematch traces each session, proposes when you correct the agent, and keeps only what later sessions confirm. Every `/rematch …` command in omp is `rematch …` in a terminal; add `--text` there for readable output instead of JSON.

```sh
rematch status --text                       # entries by status, pending flags, off?
rematch library why --text                  # last session here: what loaded, predicate hits, redirects and their class
rematch library ls --text                   # every entry with its raw counters
rematch digest --text                       # staged, kept, retired since you last looked; approvals waiting
rematch doctor --text                       # omp version, resolved roles, plugin linked, ledger

rematch mark --session <id> --class correction     # in omp: /rematch mark correction|preference|scope
rematch library approve <id>                # or: library decline <id>

rematch create <name> --from sessions|task|description
rematch improve <skill> [--split-only]
rematch task add <id> --repo <path> --prompt "…" --verifier "<cmd>"
rematch run --task <id> [--verifier "<cmd>"] [--width 2]  # detached; rematch status <job>

rematch library pin|unpin|disable <id>
rematch library rollback <name>
rematch ship --scope team|fork [--entry <id>…] [--push --pr]

rematch import --history && rematch reflect --history
rematch report --text                       # the kill criterion over the last 6 weeks
rematch lake export && rematch lake doctor --text
rematch profile init --remote <git url> [--adopt]

rematch off | rematch on                    # REMATCH=0 omp: off for one process
rematch uninstall                           # prints the plan; --yes removes
```

| For | Read |
|---|---|
| install (omp, Claude Code, Codex), the first session | [Get started](https://vncp.github.io/rematch/get-started/install.html) |
| the loop, statuses, predicates, domains, counters | [Concepts](https://vncp.github.io/rematch/concepts/loop.html) |
| day to day, correcting, approving, create, improve, batch, sharing, profile, report, lake, troubleshooting | [Guides](https://vncp.github.io/rematch/guides/day-to-day.html) |
| every subcommand's flags, trace schema, entry layout, lint, config, environment, files | [Reference](https://vncp.github.io/rematch/reference/cli.html) |
| what is built and what is not | [Status and limits](https://vncp.github.io/rematch/status.html) |
