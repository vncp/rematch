#!/bin/sh
# Fake `omp` for batch rollouts: prints a tool-enabled `--mode json` stream (one failing
# bash tool call, then a final assistant text) with provider/model from `--model`.
# Side effect standing in for agent behavior: when any SKILL.md in the overlay's
# `skills.customDirectories` contains `rematch-test-marker`, the "agent" creates
# `.rematch-marker` in its cwd (tests verify with `test -f .rematch-marker`).
# $FAKE_OMP_LOG (optional): append one line per invocation (argv).
set -eu

model=""
cwd=""
config=""
prev=""
for a in "$@"; do
  case "$prev" in
    --model) model="$a" ;;
    --cwd) cwd="$a" ;;
    --config) config="$a" ;;
  esac
  case "$a" in
    --version) echo "omp/0.0.0-fake"; exit 0 ;;
  esac
  prev="$a"
done
if [ -n "${FAKE_OMP_LOG:-}" ]; then
  printf '%s\n' "$*" >>"$FAKE_OMP_LOG"
fi
provider="${model%%/*}"
id="${model#*/}"
id="${id%%:*}"
[ -n "$cwd" ] || cwd="$(pwd)"

skills=""
if [ -n "$config" ] && [ -f "$config" ]; then
  skills=$(sed -n '/customDirectories:/{n;s/^[[:space:]]*-[[:space:]]*//;s/^["'\'']//;s/["'\'']$//;p;}' "$config")
fi
if [ -n "$skills" ] && [ -d "$skills" ] && grep -rqs 'rematch-test-marker' "$skills"; then
  : >"$cwd/.rematch-marker"
fi

usage='{"input":100,"output":20,"cacheRead":0,"cacheWrite":0,"totalTokens":120}'
printf '{"type":"session","version":3,"id":"0199f000-0000-7000-8000-%012d","timestamp":"2026-10-25T00:00:00.000Z","cwd":"%s"}\n' "$$" "$cwd"
printf '{"type":"agent_start"}\n'
printf '{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"Make the check pass in the build script"}],"timestamp":1792979906962}}\n'
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"sh build.sh"}}],"provider":"%s","model":"%s","usage":%s,"stopReason":"toolUse","timestamp":1792979907000}}\n' "$provider" "$id" "$usage"
printf '{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"sh build.sh"}}\n'
printf '{"type":"message_end","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"build.sh: check failed"}],"isError":true,"timestamp":1792979908000}}\n'
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"provider":"%s","model":"%s","usage":%s,"stopReason":"stop","timestamp":1792979909000}}\n' "$provider" "$id" "$usage"
printf '{"type":"agent_end","messages":[]}\n'
