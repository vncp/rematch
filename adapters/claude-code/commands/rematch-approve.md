---
description: rematch approve — approve one waiting action exactly as reviewed (scripts, hook, subagent, personal instructions, conflict restore, AGENTS.md sync, export, ship, probe); --sha is required and binds the snapshot you inspected — copy the command the session-start digest or /rematch:rematch-inspect prints
argument-hint: "<action id> --sha <hash>"
allowed-tools: Bash(sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh *)
disable-model-invocation: true
---

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text library approve $ARGUMENTS`

The output above is `rematch library approve` for the action the user gave. Show it to the user as it is.
