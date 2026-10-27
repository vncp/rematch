# Approving scripts and hooks

Skill *text* activates on its own. Anything executable does not:

- A proposed **script** is staged mode 0644 (not runnable). A proposed **hook** is not installed, and until you approve it nothing of it loads or counts.
- At session start the digest asks, once per item, with the diff and the hook's pass condition: approve or decline. Decline retires it.
- From the terminal: `rematch library approve <id>` / `rematch library decline <id>`.

Approved scripts become 0755 in place; approved hooks are compiled to `~/.omp/agent/extensions/rematch-hooks/<id>.ts` (data only; the extension runs them). A hook has one of four modes:

| Mode | What it does |
|---|---|
| `block` | blocks a tool call whose input field matches a pattern, with the rule's message |
| `stuck` | blocks a call identical (same tool, same arguments) to the N calls right before it; any other call in between resets it |
| `truncate` | in the request sent to the model (never in the session file), tool outputs older than N user turns keep their first K characters and end with `…[rematch: truncated N chars, age K turns]` |
| `retrieve` | before each request, appends one hidden message with the units of your loaded skill packages that match what you just asked and the files just touched, within a byte budget |

## Built-in entries

Three hook entries ship with rematch and show up as approvals the first time the digest runs (branch `shared`, they match every session):

| Entry | Mode | Setting |
|---|---|---|
| `context-unit-retrieval` | `retrieve` | ≤ 6000 bytes, units sharing ≥ 2 stems with the request |
| `tool-output-truncation` | `truncate` | outputs older than 6 user turns keep 1200 characters |
| `stuck-detector` | `stuck` | the 4th identical call in a row is blocked |

They are ordinary entries: nothing runs until you approve one; once approved, it is counted in every session, retired after two sessions in which you redirected the agent (any classed correction, preference or scope redirect) or two with harm (error cluster, takeover, disable), and kept after four clean ones. Declining retires it for good. `rematch builtins ls` shows their status; `rematch builtins reinstall [name]` stages a retired one again.

## In Claude Code and Codex

Claude Code has no approval dialog: the session-start digest lists each approval with `/rematch:rematch-approve <id>` · `/rematch:rematch-decline <id>`, which only you can run, or `rematch library approve|decline <id>` in a terminal ([Install · Claude Code](../get-started/install.md#claude-code)). Codex runs no hooks at all; the optional `$rematch-status` skill runs the digest and approvals through `rematch`, each command after your approval ([Install · Codex](../get-started/install.md#codex)).

---

Source: moved from docs/MANUAL.md ("Approving scripts and hooks", "Claude Code", "Codex"); docs/DESIGN.md §10 (hook modes, built-in entries).
