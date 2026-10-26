# Shared replies for the scripted test editors (sourced; `$input` holds the prompt).

# Classifier: every `"id": "<sid>:<seq>"` in the prompt gets class $1.
classify_all() {
  ids=$(printf '%s' "$input" | grep -o '"id": "[^"]*"' | sed 's/^"id": //') || true
  printf '```json\n['
  sep=""
  for id in $ids; do
    printf '%s{"id":%s,"class":"%s","confidence":0.9}' "$sep" "$id" "$1"
    sep=","
  done
  printf ']\n```\n'
}

reflector_reply() {
  cat <<'EOF'
```json
{"situation":"Commits in a Rust crate were made before its tests ran.","lesson":"Run the crate's tests and read the result before committing.","tag":"invariant","evidence":"origin moment; decision from counters"}
```
EOF
}

# A valid EDIT reply (passes lint): trigger line, laws, procedure in order.
clean_edit() {
  cat <<'EOF'
```json
{
  "decision": "EDIT",
  "entry": {
    "kind": "skill",
    "name": "run-tests-before-commit",
    "description": "Use when committing changes to a Rust crate: run its tests first.",
    "trigger_line": "Use when committing changes to a Rust crate.",
    "files": {
      "SKILL.md": "---\nname: run-tests-before-commit\ndescription: Use when committing changes to a Rust crate; run its tests first.\n---\n\nUse when committing changes to a Rust crate.\n\n## Laws\n\n- Run `cargo test -p <crate>` for every touched crate and read the result before any commit.\n\n## Procedure\n\n1. Run the touched crate's tests.\n2. Commit only when they pass; report failures instead of committing.\n"
    },
    "hook": null,
    "stems": ["test", "commit", "ledger"]
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "The user no longer has to ask for tests before a commit.",
    "predicted_regression": "One extra test run on trivial commits.",
    "rationale": "The agent committed before running the crate's tests and the user redirected it."
  }
}
```
EOF
}

# An EDIT reply that fails lint (URL ⇒ executable_risk error).
url_edit() {
  cat <<'EOF'
```json
{
  "decision": "EDIT",
  "entry": {
    "kind": "skill",
    "name": "run-tests-before-commit",
    "description": "Use when committing changes to a Rust crate: run its tests first.",
    "trigger_line": "Use when committing changes to a Rust crate.",
    "files": {
      "SKILL.md": "---\nname: run-tests-before-commit\ndescription: Use when committing changes to a Rust crate.\n---\n\nUse when committing changes to a Rust crate.\n\n## Laws\n\n- Follow https://example.com/testing-guide before committing.\n"
    },
    "hook": null,
    "stems": ["test", "commit"]
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "Tests run before commits.",
    "predicted_regression": "None expected.",
    "rationale": "The agent committed before running tests."
  }
}
```
EOF
}

keep_base() {
  printf '```json\n{"decision":"KEEP_BASE"}\n```\n'
}

# A valid EDIT reply shipping a script (staged 0644 until approved).
script_edit() {
  cat <<'JSON'
```json
{
  "decision": "EDIT",
  "entry": {
    "kind": "script",
    "name": "crate-test-gate",
    "description": "Use when finishing a change in a Rust crate: run the bundled test gate.",
    "trigger_line": "Use when finishing a change in a Rust crate.",
    "files": {
      "SKILL.md": "---\nname: crate-test-gate\ndescription: Use when finishing a change in a Rust crate.\n---\n\nUse when finishing a change in a Rust crate.\n\n## Procedure\n\n1. Run `sh scripts/check.sh <crate>` and read its output before reporting.\n",
      "scripts/check.sh": "#!/bin/sh\nset -eu\ncargo test -p \"$1\"\n"
    },
    "hook": null,
    "stems": ["test", "crate"]
  },
  "manifest": {
    "evidence": {"session_id": "", "seq": 0},
    "predicted_fix": "Changes are tested before they are reported done.",
    "predicted_regression": "A slow test suite delays hand-off.",
    "rationale": "Deterministic test gate instead of prose."
  }
}
```
JSON
}
