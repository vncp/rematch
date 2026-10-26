#!/bin/sh
# Editor answers with a subagent entry (kind agent: AGENT.md, no SKILL.md).
set -eu
input=$(cat)
if [ -n "${EDITOR_LOG:-}" ]; then printf '%s\n' "${REMATCH_PROMPT:-?}" >>"$EDITOR_LOG"; fi
. "$(dirname "$0")/editor-common.sh"
case "${REMATCH_PROMPT:-editor}" in
  classifier) classify_all correction ;;
  reflector) reflector_reply ;;
  *) cat <<'JSON'
```json
{
  "decision": "EDIT",
  "entry": {
    "kind": "agent",
    "name": "log-triage",
    "description": "Use for a long build or test log: returns the first failing step and its error line.",
    "trigger_line": "Use for a long build or test log.",
    "files": {
      "AGENT.md": "---\nname: log-triage\ndescription: Use for a long build or test log; returns the first failing step and its error line.\nmodel: \"@smol\"\ntools: read, grep\n---\n\nYou receive the path of a build or test log. Read it, find the first failing step and reply with that step's name and its first error line. Nothing else.\n"
    },
    "hook": null,
    "stems": ["triag", "build", "log"]
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "Long logs are read in a subagent instead of the main context.",
    "predicted_regression": "A failing step outside the first error is missed.",
    "rationale": "Large input, small answer: the log stays out of the main context."
  }
}
```
JSON
  ;;
esac
