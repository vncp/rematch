#!/bin/sh
# Claude Code `statusLine` command (`settings.json` → `{"statusLine":{"type":"command","command":"..."}}`).
# Forwards the statusLine JSON payload on stdin to `rematch trace-event --harness claude-code --hook-output`
# and prints the single ANSI status line (`◆ rematch  ✓ cargo 0.1s · 5 on trial · ● 1 needs you`).
here=$(cd "$(dirname "$0")" && pwd -P)
exec sh "$here/hook.sh" status-line
