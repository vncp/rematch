#!/bin/sh
# Scripted editor for batch tests: reads the editor prompt on stdin and answers by scope.
# invariant → a process law without the marker; domain and compose → an entry whose
# SKILL.md mentions `rematch-test-marker` (fake-omp-batch.sh then creates the marker).
set -eu
input=$(cat)

reply() {
  name="$1"
  law="$2"
  cat <<EOF
\`\`\`json
{"decision":"EDIT",
 "entry":{"kind":"skill","name":"$name","description":"Use when finishing a build script task in this repository.",
  "trigger_line":"Use when finishing a build script task in this repository.",
  "files":{"SKILL.md":"---\nname: $name\ndescription: Use when finishing a build script task in this repository.\n---\n\nUse when finishing a build script task in this repository.\n\n## Laws\n- $law\n"},
  "hook":null,"stems":["build","script"]},
 "manifest":{"evidence":{"session_id":"not-in-trace","seq":99},"predicted_fix":"The verifier check stops failing.",
  "predicted_regression":"An extra file is created in unrelated tasks.","rationale":"The reference run ended without the checked file."}}
\`\`\`
EOF
}

case "$input" in
  *'"scope": "compose"'*) reply composed-check "MUST create the rematch-test-marker file and rerun the build before yielding." ;;
  *'"scope": "invariant"'*) reply verify-before-yield "MUST rerun the failing command after each fix before yielding." ;;
  *'"scope": "domain"'*) reply check-marker-before-yield "MUST create the rematch-test-marker file before yielding." ;;
  *) echo '```json'; echo '{"decision":"KEEP_BASE"}'; echo '```' ;;
esac
