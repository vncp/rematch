---
title: The eight surfaces
description: The eight parts of a harness rematch can change, and what each needs before it activates.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

A harness has eight parts an improver can edit. rematch maps each to one omp component and gives each its own activation rule: skill, rule and fact text in your personal scope activate automatically as scoped provisional trials; personal `AGENTS.md` instructions, subagents, scripts and hooks wait in the persistent action queue for your approval; anything beyond your machine waits for a branch or PR.

Who approves what, and how team and fork changes leave your machine: [the evidence boundary](/rematch/concepts/evidence-boundary/#scope-and-who-merges).

## Surface by surface

:::note[Generated]
From `docs/DESIGN.md` §16 (region `surfaces`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

Harness² edits eight surfaces (research D3). Each maps to one omp component; activation follows SPEC §"Scope and who merges" as amended by D29–D32: skill, rule and fact text activate automatically as scoped `provisional` trials; personal instructions, subagents, scripts and hooks wait in the persistent action queue (§14 "Actions and approvals") for an approval bound to the reviewed snapshot; team and fork changes leave only as a branch/PR the binary never merges.

| Surface | omp component | Mechanism in rematch | Activation | Status |
| --- | --- | --- | --- | --- |
| System prompt | `<agent_dir>/AGENTS.md` (personal user context file in omp); `<repo>/AGENTS.md` (team); per-model prompt text in the omp fork | `AGENTS.fragment.md` in an entry → personal (`instructions:<id>`): written as a marked `<!-- rematch:begin name=<name> ... -->` block in `<agent_dir>/AGENTS.md`, which omp loads natively (no runtime injection in omp; synced across machines as `profile/instructions/<name>.md`), and inserted at runtime in matched main sessions of Claude Code (`SessionStart` `additionalContext`), Pi (`before_agent_start`) and OpenCode (`experimental.chat.system.transform`), each recording `fragments_sent`; Codex: not supported; team (`ship-team:<id>`): section `## rematch: <name>` of `<repo>/AGENTS.md` (`ship --scope team`); fork (`fork:<id>`): prompt files via `fork_path` (`ship --scope fork`) | personal: approval of `instructions:<id>` bound to the reviewed snapshot; declining keeps the entry; applied blocks are listed and withdrawable (`instructions-applied:<name>`, profile-wide with a profile); user edits inside a block surface `instructions-conflict:<name>`; team / fork: local branch on approval, PR via `ship --push --pr` | built; `<repo>/AGENTS.md` is never written by personal adoption; bytes outside rematch markers in `<agent_dir>/AGENTS.md` are preserved; all blocks together within `always_on_bytes` (8,192 B), checked at approval and at every sync |
| Guardrails / rules | laws in a triggered skill's router `SKILL.md` | `## Laws` of a skill entry (`kind: rule` entries are skills); lint form order | automatic as `provisional`, kept by accepted-in-use or comparative support | built; standalone always-on rule files deferred (they would cost every session) |
| Memory / facts | `facts/*.md` inside a skill (each with `as_of`), `EXPERIENCE.md` beside entries | fact entries and units; `stale` on source drift; the reflector writes EXPERIENCE.md after keep/retire and the editor reads matching records | automatic; domain facts never transfer | built; omp's own memory backend is not written |
| Tool guidance | the fork's tool descriptions / rules text | `fork_path` (`library set-fork-path` or `entry.fork_path` from the editor) + `fork/<basename>` → `ship --scope fork` (`fork:<id>`) | fork branch / PR only | built |
| Subagents | `~/.omp/agent/agents/<name>.md` (user), `<extension-root>/agents/*.md` | `kind: agent` entry (`AGENT.md` + `rematch.json`); provisional, once approved: linked in `REMATCH_ROOT/agents/<name>.md` (omp hides unmatched places via `task.disabledAgents`) and copied to `<repo>/.claude/agents/<name>.md` (git-excluded) for matching Claude Code repos; kept: copied to `~/.omp/agent/agents/<name>.md` (with a profile: `profile/agents/<name>.md`) and `<claude-plugin>/agents/<name>.md`; team: `.omp/agents/<name>.md` | approval of `<entry-id>` (bound to `AGENT.md` sha256 without its `rematch:` block); scoped to matching places while provisional; withdrawn on `off` | built |
| Skills | `~/.omp/agent/skills/<name>/SKILL.md` | kept/pinned linked in `REMATCH_ROOT/skills/`; provisional linked only in the matching place's view `REMATCH_ROOT/views/<key>/skills/` (and `views/<key>/trials/` for Codex); kept: copied natively to `~/.omp/agent/skills/<name>/` (with a profile: `profile/skills/<name>/`); team: `.omp/skills/<name>/` | automatic as scoped `provisional` trial (`description` ≤ 1,024 B, catalog ≤ 8,192 B, router body ≤ 8,192 B) | built |
| Scripts | `scripts/<n>` inside a skill | staged mode 0644; `library approve <entry-id>` makes them 0755 | explicit approval | built |
| Hooks / plugins | `~/.omp/agent/extensions/rematch-hooks/<id>.ts` (data-only modules; with a profile links to `profile/hooks/<id>.ts`, §20), imported by the extension and run on `tool_call` (`block`, `stuck`) or `context` (`truncate`, `retrieve`) | `kind: hook` entry (`hook.json` with a `mode`), compiled on approval; the three built-in entries (§10) are hook entries of the context modes | explicit approval | built: block verifiers, stuck detection, age-based tool-output truncation, harness-driven unit retrieval; other plugin shapes (error hints) are not generated |

## See also

- [Approve scripts and hooks](/rematch/guides/approve-scripts-and-hooks/) and [share to a team or fork](/rematch/guides/share/).
- [Entry layout](/rematch/reference/entry-layout/): the files each surface lives in.
- Why eight: [D3](/rematch/background/decisions/).
