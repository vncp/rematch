#!/bin/sh
# Editor answers with a skill that also carries an AGENTS.md fragment and a fork_path.
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
    "kind": "skill",
    "name": "crate-tests-first",
    "description": "Use when committing changes to a Rust crate: run its tests first.",
    "trigger_line": "Use when committing changes to a Rust crate.",
    "files": {
      "SKILL.md": "---\nname: crate-tests-first\ndescription: Use when committing changes to a Rust crate; run its tests first.\n---\n\nUse when committing changes to a Rust crate.\n\n## Laws\n\n- Run `cargo test -p <crate>` for every touched crate and read the result before any commit.\n",
      "AGENTS.fragment.md": "Run `cargo test -p <crate>` for every crate you touched before you commit.\n\n### Why\n\nCommits without a test run were the most common correction here.\n"
    },
    "hook": null,
    "stems": ["test", "commit", "crate"],
    "fork_path": "docs/prompts/team.md"
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "Tests run before commits for every contributor.",
    "predicted_regression": "One extra test run on trivial commits.",
    "rationale": "The rule applies to every run in this repo, so it belongs in AGENTS.md through the team PR."
  }
}
```
JSON
  ;;
esac
