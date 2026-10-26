#!/bin/sh
# Fake `omp` for tests: prints a canned `--mode json` event stream.
# Reply text: $FAKE_OMP_REPLY_FILE (whole file) or a default. Provider/model come from
# the `--model provider/id` argument. `--version` prints a version line.
# $FAKE_OMP_LOG (optional): append one line per invocation (argv up to `--`, then the
# prompt's byte count) for call counting.
set -eu

model=""
prev=""
for a in "$@"; do
  case "$a" in
    --version) echo "omp/0.0.0-fake"; exit 0 ;;
    --model=*) model="${a#--model=}" ;;
  esac
  if [ "$prev" = "--model" ]; then model="$a"; fi
  prev="$a"
done
if [ -n "${FAKE_OMP_LOG:-}" ]; then
  line=""
  for a in "$@"; do
    if [ "$a" = "--" ]; then break; fi
    line="$line $a"
  done
  last=""
  for a in "$@"; do last="$a"; done
  printf '%s -- <%s bytes>\n' "${line# }" "$(printf '%s' "$last" | wc -c | tr -d ' ')" >>"$FAKE_OMP_LOG"
fi
provider="${model%%/*}"
id="${model#*/}"
id="${id%%:*}"

if [ -n "${FAKE_OMP_REPLY_FILE:-}" ] && [ -f "$FAKE_OMP_REPLY_FILE" ]; then
  reply=$(cat "$FAKE_OMP_REPLY_FILE")
else
  reply='```json
{"decision":"KEEP_BASE"}
```'
fi

# JSON-escape the reply (backslash, quote, newline, tab, CR).
esc=$(printf '%s' "$reply" | awk 'BEGIN{ORS=""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); gsub(/\t/,"\\t"); gsub(/\r/,"\\r"); if (NR>1) print "\\n"; print}')

msg="{\"role\":\"assistant\",\"provider\":\"$provider\",\"model\":\"$id\",\"content\":[{\"type\":\"text\",\"text\":\"$esc\"}],\"stopReason\":\"stop\",\"usage\":{\"input\":10,\"output\":20,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":30}}"
printf '{"type":"message_start","message":%s}\n' "$msg"
printf '{"type":"message_end","message":%s}\n' "$msg"
printf '{"type":"agent_end","messages":[%s]}\n' "$msg"
