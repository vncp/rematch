# The evidence boundary

The session is the evidence. The extension writes every input, tool result and turn to a trace file of its own, and at shutdown flags the session only if it carried a signal: a redirect, a cluster of tool errors, a takeover, or an explicit mark. No proposal exists without that trace (D1).

rematch is an omp plugin: a TypeScript extension that omp loads, and a Rust binary the extension calls with a JSON envelope. The extension only records and toggles; the binary proposes and stages. The boundary between what a model may propose and what the system may write is enforced by record types, not by a prompt: proposals carry no score field, and nothing activates without a lint report and, for anything executable, a receipt (D24). Skill text activates in personal scope on its own. Anything executable (scripts, hooks) and anything beyond personal scope (team `.omp/`, your fork) waits for a one-tap approval or a merged PR (D4, D3). No fork of omp is required; the fork is one of the things it can propose changes to.

## The eight surfaces

{{#include ../../../DESIGN.md:surfaces}}

## Scope and who merges

| Scope | Activation | Examples |
|---|---|---|
| Personal, text | automatic as `provisional` | skill text, laws, facts, lessons, subagents |
| Personal, executable or blocking | one tap in the session-start digest, with the diff | scripts (0644 → 0755), compiled `tool_call` verifiers |
| Team (`<repo>/.omp/`, `<repo>/AGENTS.md`) | `ship --scope team`: a branch with the entry (skills in `.omp/skills/`, subagents in `.omp/agents/`, an `AGENTS.fragment.md` as a section of `AGENTS.md`) and an evidence commit; `--push --pr` opens the PR, never merges | entries whose predicate recurred across two or more repos, or that you list; the only way text reaches `AGENTS.md` |
| Fork | `ship --scope fork [--repo <fork>]`: the entry's `fork_path` (set with `library set-fork-path`), the built fingerprint and before/after counters in the PR body | per-model prompt text, tool descriptions, compaction defaults |
| Work → home | `library export`, per-entry approval with the diff; off by default at work (`policy.json`) | invariant entries only; lint with `transfer: true` blocks domain and workspace facts |

A branch is a repo or project. `shared` holds entries kept in two or more branches; promotion needs a second branch's canary and no new canary of its own. When the shared copy replaces a kept entry whose originating task has a verifier, that task is replayed once with the copy in place first; a failing replay retires the copy and the kept entry stays. `transfer suggest` lists kept invariant lessons not yet tried in a branch whose facet structure overlaps on two or more keys (`branches/<branch>/domain.yml`); a transfer is applied as `provisional` and earns its place through that branch's counters.

---

Source: README.md ("How it works", "Scope and who merges"); docs/DESIGN.md §16 (included); docs/research.md D1, D3, D4, D24.
