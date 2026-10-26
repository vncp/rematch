#!/bin/sh
# Claude Code hook relay: forwards the hook payload on stdin, unchanged, to
#   rematch trace-event|hook-check --harness claude-code --hook-output
# The binary owns the trace schema and prints the hook's JSON reply. This relay never fails a
# hook: every error exits 0; only an intentional hook-check block (exit 2, the reason on
# stderr, nothing on stdout) passes through.
op=$1
case "$op" in
	trace-event | hook-check) ;;
	*) exit 0 ;;
esac
here=$(cd "$(dirname "$0")" && pwd -P)
out=$(sh "$here/rematch.sh" "$op" --harness claude-code --hook-output)
code=$?
if [ "$code" -eq 2 ] && [ -z "$out" ]; then
	exit 2
fi
if [ "$code" -eq 0 ] && [ -n "$out" ]; then
	printf '%s\n' "$out"
fi
exit 0
