# rematch manual

The manual is the documentation site, **<https://vncp.github.io/rematch/>**, built from [`docs/site/`](site/) (`cd docs/site && bun install && bun run build`). Its guides hold everything this file used to; below is a quick reference.

**Use your connected harness as usual.** The live adapters trace sessions and can propose changes from corrections; Codex is post-hoc. The CLI below works across adapters. Slash-command names vary—see the [harness matrix](https://vncp.github.io/rematch/reference/harnesses/). Add `--text` for readable terminal output.

```sh
rematch status --text                       # entries by status, actions, trials, probes, jobs, off?
rematch library why --text                  # last session here: what loaded, personal instructions, hits, redirects
rematch library ls --text                   # every entry with counters, plus actions, trials and instructions
rematch digest --text                       # status changes since last time; persistent pending actions
rematch doctor --text                       # omp version, resolved roles, plugin linked, ledger

rematch mark --session <id> --class correction     # in omp: /rematch mark correction|preference|scope
rematch library inspect <action-id> --text         # full diff, target, reach, and the approve/decline commands
rematch library approve <action-id> --sha <hash>   # the command inspect prints; decline is bound the same way

rematch create <name> --from sessions|task|description
rematch improve <skill> [--split-only]
rematch task add <id> --repo <path> --prompt "…" --verifier "<cmd>" [--editable <path>… | --tools read,grep,glob]

rematch library pin|unpin|disable <id>
rematch library rollback <name>
rematch ship --scope team|fork [--entry <id>…] [--push --pr]

rematch import --history && rematch reflect --history
rematch report --text                       # the kill criterion (≥ 4 clean actual uses, recurrence, comparative support)
rematch lake export && rematch lake doctor --text
rematch profile init --remote <git url> [--adopt]

rematch off | rematch on                    # REMATCH=0 omp: off for one process
rematch uninstall                           # prints the plan; --yes removes
```

| For | Read |
|---|---|
| install (omp, Claude Code, Codex, Pi, OpenCode), the first correction | [Start](https://vncp.github.io/rematch/start/install/) |
| the loop, statuses, predicates, domains, counters | [Concepts](https://vncp.github.io/rematch/concepts/loop/) |
| day to day, correcting, approving, automatic probes, create, improve, batch, sharing, profile, report, lake, troubleshooting | [Guides](https://vncp.github.io/rematch/guides/day-to-day/) |
| every subcommand's flags, trace schema, entry layout, lint, config, environment, files, glossary | [Reference](https://vncp.github.io/rematch/reference/cli/) |
| what is built and what is not | [Status and limits](https://vncp.github.io/rematch/background/status/) |
