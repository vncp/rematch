---
description: rematch mark — classify the last turn of this session as a correction, preference or scope redirect
argument-hint: "correction|preference|scope"
allowed-tools: Bash(sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh *)
disable-model-invocation: true
---

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text mark --session cc:${CLAUDE_SESSION_ID} --class $ARGUMENTS`

The output above is `rematch mark` for this session. Show it to the user as it is.
