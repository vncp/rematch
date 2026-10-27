---
title: The eight surfaces
description: The eight parts of a harness rematch can change, and what each needs before it activates.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

A harness has eight parts an improver can edit. rematch maps each to one omp component and gives each its own activation rule: text in your personal scope activates as provisional, anything executable waits for your approval, anything beyond your machine waits for a merged PR.

Who approves what, and how team and fork changes leave your machine: [the evidence boundary](/rematch/concepts/evidence-boundary/#scope-and-who-merges).

## Surface by surface

:::note[Generated]
From `docs/DESIGN.md` §16 (region `surfaces`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

Harness² edits eight surfaces (research D3). Each maps to one omp component; activation follows SPEC §"Scope and who merges" (personal text automatic as `provisional`; executable or blocking behind a one-tap approval; team and fork only as a branch/PR the binary never merges).

| Surface | omp component | Mechanism in rematch | Activation | Status |
| --- | --- | --- | --- | --- |
| System prompt | `<repo>/AGENTS.md` (context file); per-model prompt text in the omp fork | `AGENTS.fragment.md` in an entry → section `## rematch: <name>` of `<repo>/AGENTS.md` (`ship --scope team`); fork prompt files via `fork_path` (`ship --scope fork`) | team / fork PR only | built; never written locally (always-on for everyone in the repo, so it needs review) |
| Guardrails / rules | laws in a triggered skill's router `SKILL.md` | `## Laws` of a skill entry (`kind: rule` entries are skills); lint form order | automatic as `provisional`, counters keep or retire | built; standalone always-on rule files deferred (they would cost every session) |
| Memory / facts | `facts/*.md` inside a skill (each with `as_of`), `EXPERIENCE.md` beside entries | fact entries and units; `stale` on source drift; the reflector writes EXPERIENCE.md after keep/retire and the editor reads matching records | automatic; domain facts never transfer | built; omp's own memory backend is not written |
| Tool guidance | the fork's tool descriptions / rules text | `fork_path` (`library set-fork-path` or `entry.fork_path` from the editor) + `fork/<basename>` → `ship --scope fork` | fork PR only | built |
| Subagents | `~/.omp/agent/agents/<name>.md` (user), `<extension-root>/agents/*.md` | `kind: agent` entry (`AGENT.md` + `rematch.json`); provisional: `extension/agents → REMATCH_ROOT/agents/<name>.md → staging/<branch>/<name>/AGENT.md`; kept: copied to `~/.omp/agent/agents/<name>.md` (predecessor saved; with a profile copied to `profile/agents/<name>.md` and linked, §20); team: `.omp/agents/<name>.md` | automatic as `provisional` (visible in every session, counted where the glob hits) | built |
| Skills | `~/.omp/agent/skills/<name>/SKILL.md` | linked into the shared dir `REMATCH_ROOT/skills/` (below), which the factory adds to `skills.customDirectories`; kept: copied natively to `~/.omp/agent/skills/<name>/` (with a profile: to `profile/skills/<name>/`, linked from there, §20) and linked into the shared dir too (one skill by realpath, §0); team: `.omp/skills/<name>/` | automatic as `provisional` | built |
| Scripts | `scripts/<n>` inside a skill | staged mode 0644; `library approve` (or the digest's one tap) makes them 0755 | one tap | built |
| Hooks / plugins | `~/.omp/agent/extensions/rematch-hooks/<id>.ts` (data-only modules; with a profile links to `profile/hooks/<id>.ts`, §20), imported by the extension and run on `tool_call` (`block`, `stuck`) or `context` (`truncate`, `retrieve`) | `kind: hook` entry (`hook.json` with a `mode`), compiled on approval; the three built-in entries (§10) are hook entries of the context modes | one tap | built: block verifiers, stuck detection, age-based tool-output truncation, harness-driven unit retrieval; other plugin shapes (error hints) are not generated |

## See also

- [Approve scripts and hooks](/rematch/guides/approve-scripts-and-hooks/) and [share to a team or fork](/rematch/guides/share/).
- [Entry layout](/rematch/reference/entry-layout/): the files each surface lives in.
- Why eight: [D3](/rematch/background/decisions/).
