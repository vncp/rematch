#!/bin/sh
# Scripted editor for the smoke test (`rematch create --editor`): stages a `block` hook entry
# that refuses `bash` commands containing `rm -rf` (no model call; the reply is fixed).
cat >/dev/null
cat <<'JSON'
```json
{
  "decision": "EDIT",
  "entry": {
    "kind": "hook",
    "name": "no-recursive-delete",
    "description": "Use when a shell command would delete a directory tree: refuse rm -rf.",
    "trigger_line": "Use when a shell command would delete a directory tree.",
    "files": {
      "SKILL.md": "---\nname: no-recursive-delete\ndescription: Use when a shell command would delete a directory tree; refuse rm -rf.\n---\n\nUse when a shell command would delete a directory tree.\n\n## Laws\n\n- Never run `rm -rf`; move the directory aside or ask the user.\n"
    },
    "hook": {"mode": "block", "tool": "bash", "field": "command", "regex": "rm\\s+-rf", "message": "rematch: rm -rf is blocked by an approved hook; ask the user instead.", "pass_condition": "Every command without rm -rf passes."},
    "stems": ["delete", "directory", "shell"]
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "No recursive deletes run without the user.",
    "predicted_regression": "A legitimate cleanup needs the user's go-ahead.",
    "rationale": "A recursive delete removed work the user wanted kept."
  }
}
```
JSON
