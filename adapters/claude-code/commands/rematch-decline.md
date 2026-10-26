---
description: rematch decline — decline an approval (retires the entry; `export:<id>` only suppresses the offer)
argument-hint: "<entry id | export:<id>>"
allowed-tools: Bash(sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh *)
disable-model-invocation: true
---

!`sh ${CLAUDE_PLUGIN_ROOT}/scripts/rematch.sh --text library decline $ARGUMENTS`

The output above is `rematch library decline` for the id the user gave. Show it to the user as it is.
