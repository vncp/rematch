---
description: rematch approve — approve everything an entry waits for (scripts, hook, subagent, personal instructions; the id from the session-start digest)
argument-hint: "<entry id | export:<id>>"
allowed-tools: Bash(sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh *)
disable-model-invocation: true
---

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text library approve $ARGUMENTS`

The output above is `rematch library approve` for the id the user gave. Show it to the user as it is.
