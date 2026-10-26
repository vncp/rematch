#!/bin/sh
# Claude Code hook relay: forwards the hook payload on stdin, unchanged, to
#   rematch trace-event|hook-check --harness claude-code --hook-output
# The binary owns the trace schema and prints the hook's JSON reply. This relay never fails a
# hook: every error exits 0; only an intentional hook-check block (exit 2, the reason on
# stderr, nothing on stdout) passes through.
#
# `session-start` is `trace-event`, then `rematch reflect --pending` started in the
# background, as the omp extension does at session start: detached (own process group, no
# stdio), so it never delays the hook and outlives the session; the binary's `pending/.lock`
# keeps it to one run at a time. It starts after the `session_start` record is written, so a
# resumed session's trace is unfinished again and is not counted half-way. The reflection's
# model calls go through omp, so nothing is started without it ($OMP_BIN, else `omp` on
# PATH): a failed call would use up the flagged session's attempts.
op=$1
here=$(cd "$(dirname "$0")" && pwd -P)
reflect=
case "$op" in
	trace-event | hook-check) ;;
	session-start)
		op=trace-event
		if [ -n "${OMP_BIN:-}" ]; then
			[ -x "$OMP_BIN" ] && reflect=1
		else
			command -v omp >/dev/null 2>&1 && reflect=1
		fi
		;;
	*) exit 0 ;;
esac
out=$(sh "$here/rematch.sh" "$op" --harness claude-code --hook-output)
code=$?
if [ -n "$reflect" ]; then
	set -m
	nohup sh "$here/rematch.sh" reflect --pending </dev/null >/dev/null 2>&1 &
	set +m
fi
if [ "$code" -eq 2 ] && [ -z "$out" ]; then
	exit 2
fi
if [ "$code" -eq 0 ] && [ -n "$out" ]; then
	printf '%s\n' "$out"
fi
exit 0
