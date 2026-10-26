#!/bin/sh
# First editor call emits an entry with a URL (lint error); the retry (input carries
# `retry_findings`) emits a clean entry.
set -eu
input=$(cat)
if [ -n "${EDITOR_LOG:-}" ]; then printf '%s\n' "${OMP_EVOLVE_PROMPT:-?}" >>"$EDITOR_LOG"; fi
. "$(dirname "$0")/editor-common.sh"
case "${OMP_EVOLVE_PROMPT:-editor}" in
  classifier) classify_all correction ;;
  reflector) reflector_reply ;;
  *)
    if printf '%s' "$input" | grep -q '"retry_findings": \['; then clean_edit; else url_edit; fi
    ;;
esac
