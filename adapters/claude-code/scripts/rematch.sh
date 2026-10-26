#!/bin/sh
# Runs the rematch binary with the given arguments: $REMATCH_BIN, else
# <repo>/target/{release,debug}/rematch beside this plugin, else `rematch` on PATH (the omp
# extension's order). Exit 127 with a message on stderr when none is found.
here=$(cd "$(dirname "$0")" && pwd -P)
if [ -n "${REMATCH_BIN:-}" ]; then
	[ -x "$REMATCH_BIN" ] || { echo "rematch: REMATCH_BIN=$REMATCH_BIN is not executable" >&2; exit 127; }
	exec "$REMATCH_BIN" "$@"
fi
for profile in release debug; do
	candidate="$here/../../../target/$profile/rematch"
	[ -x "$candidate" ] && exec "$candidate" "$@"
done
command -v rematch >/dev/null 2>&1 && exec rematch "$@"
echo "rematch: binary not found (\$REMATCH_BIN, target/{release,debug}, PATH)" >&2
exit 127
