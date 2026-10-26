#!/bin/sh
# Editor answers with a script-shipping entry.
set -eu
input=$(cat)
if [ -n "${EDITOR_LOG:-}" ]; then printf '%s\n' "${REMATCH_PROMPT:-?}" >>"$EDITOR_LOG"; fi
. "$(dirname "$0")/editor-common.sh"
case "${REMATCH_PROMPT:-editor}" in
  classifier) classify_all correction ;;
  reflector) reflector_reply ;;
  *) script_edit ;;
esac
