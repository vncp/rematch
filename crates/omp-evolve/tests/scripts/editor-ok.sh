#!/bin/sh
# Scripted model for tests (`--editor`): the prompt arrives on stdin; OMP_EVOLVE_PROMPT
# names the system prompt (classifier | editor | reflector). Each call appends one line
# to $EDITOR_LOG when set.
set -eu
input=$(cat)
if [ -n "${EDITOR_LOG:-}" ]; then printf '%s\n' "${OMP_EVOLVE_PROMPT:-?}" >>"$EDITOR_LOG"; fi
. "$(dirname "$0")/editor-common.sh"
case "${OMP_EVOLVE_PROMPT:-editor}" in
  classifier) classify_all correction ;;
  reflector) reflector_reply ;;
  *) clean_edit ;;
esac
