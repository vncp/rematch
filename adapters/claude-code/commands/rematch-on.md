---
description: rematch on — resume tracing, reflection and skill loading, and re-link the skills for this directory
allowed-tools: Bash(sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh *)
disable-model-invocation: true
---

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text on`

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text skills sync --harness claude-code`

The two outputs above are `rematch on` and `rematch skills sync --harness claude-code` for this directory. Show them to the user as they are.
