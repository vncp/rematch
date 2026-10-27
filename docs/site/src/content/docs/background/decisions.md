---
title: Research decisions
description: D1–D28, every design decision with the evidence and paper behind it.
editUrl: https://github.com/vncp/rematch/edit/main/docs/research.md
tableOfContents: false
---

Every design claim on this site carries a decision id. The `improver/` prompts cite them too. The papers behind each source and the Harness² prompts are in [research provenance](/rematch/background/research/).

:::note[Generated]
From `docs/research.md` §2 (region `decisions`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

| # | Decision in the v2 spec | Evidence | Source |
| --- | --- | --- | --- |
| D1 | No proposal without an execution trace | Editing the harness directly, without reference executions or probes, degraded performance: WB-code 73.4 → 63.2, WB-web 69.9 → 65.0 | Harness² §4, Fig. 7 left |
| D2 | Skills and scripts first; hooks and prompts last (lint form order) | Removing skills from the improved harness cost 5.3 points, scripts 3.7, all prompt text 1.5 (Gemini 3.5 Flash, OpenCode, k=5). Skills and scripts were ~half of all edits | Harness² Table 6, Fig. 10 |
| D3 | All eight surfaces editable | Harness² exposes system prompt, guardrails, memory, tool guidance, subagents, skills, scripts, plugin; AHE's seven file-typed components; the plugin/hook slot is "the only lever that can enforce something the model cannot skip" | Harness² §2.1, Table 5; AHE |
| D4 | Compiled hooks for recurring corrections, not prose | The same user-correction rules in context: 55.0% compliance; compiled into applicability check + instruction + verifier hooks: 70.1% | Trace (arXiv 2606.13174) |
| D5 | One scoped editor in interactive mode; invariant vs domain scope | Scoping ablation was Harness²'s largest: without it −2.9 (WorkBuddy) and −3.7 (JobBench) | Harness² Table 3 |
| D6 | Composer only in batch mode, where probes exist | Composition ablation −1.6/−1.9; but a composer choosing between untested texts is blind selection, and blind selection produced Harness²'s worst regressions (−41, −30, −28) | Harness² Table 3, Fig. 14; reviewer finding 4 |
| D7 | Verifier picks in batch mode; no LLM selector | Three of nine Harness² regressions were the selector choosing a worse candidate (55 over 95, 70 over 100, 26 over 54) | Harness² Fig. 14 |
| D8 | Provisional by default; "kept", never "verified" | Edits written by a frontier model without validation were net negative: 21 of 39 valid patches lowered batch success; an untrained Gemini editor reduced ALFWorld 41.6 → 35.4 | Harness-R1 (arXiv 2608.02276) |
| D9 | Failure-only reflection; no self-judged positives | Self-generated feedback scored 27.96/61.67, below no-evolution, in Evo-Harness; sparse pass/fail feedback (67.33) ≈ verbose (67.00) | Evo-Harness (arXiv 2608.15071) |
| D10 | Counters measure recurrence of the motivating redirect class | Free signals vary tenfold by task (tokens, errors); recurrence of a specific redirect is the one signal that is specific and hard to fake; user corrections define misalignment in the 20k-session study | Reviewer finding 7; How Coding Agents Fail Their Users (arXiv 2605.29442) |
| D11 | No on/off withholding arm in Phase 1 | Withholding a fix the user asked for both harms them and inflates the comparison; and pi's `before_agent_start` cannot hide one skill | Reviewer findings 1, 8 (verified against pi 0.80.9 `docs/extensions.md`) |
| D12 | Deterministic trigger predicate per entry | Counters are meaningless unless "matched" is computed identically for every session | Reviewer finding 2 |
| D13 | Caps forcing merges; retirement; archive never delete | Every unbounded library peaked then declined: Recuris added 51 skills, retired 0, kept 17 near-duplicates; Adaptive Auto-Harness prompt 2 KB → 68 KB then decline; Evo-Harness caps 5 general + 5 per topic | Recuris (2608.24876); Adaptive Auto-Harness (2606.01770); Evo-Harness |
| D14 | Append itemized units with counters; never let a model rewrite the library | Dynamic Cheatsheet's monolithic rewrite collapsed 18,282 tokens @ 66.7 to 122 tokens @ 57.1, below the 63.7 baseline | ACE (arXiv 2510.04618) |
| D15 | Lint: no task ids, no string-match branches, applicability clause required | HarnessCompass's generalization gate: seed 51.8 → 61.0 total with held-out 51.6 → 60.4, vs AHE 55.5 in 20 turns; gate alone 58.8 in 2 turns | HarnessCompass (arXiv 2608.01918) |
| D16 | Per-edit manifest with predicted fixes and predicted regressions | AHE's change manifests, scored next round; fix precision/recall 33.7%/51.4% vs regression 11.8%/11.1% shows regression blindness is the risk | AHE (arXiv 2604.25850) |
| D17 | Batch winners provisional until the next real run passes | Single-rollout acceptance has no noise floor; an unchanged harness scored 43.4–55.4 across 10 runs on ARC-AGI-3; Recuris measured a 25.9% A/A flip floor | arXiv 2610.30499; Recuris |
| D18 | Kill criterion for the project; no benchmark harness as prerequisite | 54% of reported harness gains were overfitting and validation did not prevent it; offline evolution generalized +0.0 on disjoint TB2.1 splits | Harness-Delta Attribution (2610.22466); Wang et al. (2607.12227) |
| D19 | Expect low single digits for strong models, more for weak ones | Deployed-static evidence: library without probing +0.9/+4.4/+5.1; cross-benchmark transfer +0.5 to +2.9; GLM-5 gained 17.3 from a transferred harness, Opus 4.8 +2.6/+1.1 | Harness² Fig. 7, Fig. 12 |
| D20 | Record model family, don't gate on it; editor = strongest configured | Same-model editing worked for all five models tested (Opus 5 +3.3/+5.5/+3.0); strong builder for weak target: ToM 0.488 → 0.912 | Harness² Fig. 8; AI4AI (2608.12307) |
| D21 | Scripts offload reasoning into code; track deterministic fraction | Deterministic fraction vs accuracy r = 0.72; 94% of BigToM answered by code with no model call | AI4AI |
| D22 | Skill packages: short router, units, facts with `as_of`, harness-driven retrieval | A loaded skill body costs ~50% more episode tokens; on-demand loading keeps ~51% of the gain; adding a checkable rule +0.41 compliance (2,608 SKILL.md revisions). Call-time retrieval of skills beat model-controlled invocation of the same skills 83.6 vs 65.6 (+18.0); recall tools were never invoked in 56.3% of settings when left to the model | Agent Skill Evolution (2610.04832); Recuris; Fan et al. (2609.20804) |
| D23 | Context truncation and stuck detector are entries, not always-on changes | Age-based tool-output truncation + stuck detector: F2PF 28% → 49% at 20k tokens, converging at 262k; cheap rule-based elision before any LLM summarization | Lewis (2608.26218); Fan et al. |
| D24 | Trust boundary by record types; proposals carry no score; receipts need certificates | Bellwether ADR 009/004/005: "the boundary is enforced by the record types, not by a prompt" | bellwether `docs/evolve.md`, README |
| D25 | Experience library read by editors; improver prompts not self-amended | Library ablation −3.1/−1.8 (reading experience helps); a self-editing meta-level "may evolve far more rapidly than humans can audit" | Harness² Table 3; Hyperagents (2603.19461); reviewer finding 18 |
| D26 | Verifiers never evolve by agent score | Verifier selected without anchors collapsed to always-pass while downstream scores still rose; self-evolution without execution-based verification scored 31.79 < blank harness 33.95 | Who Verifies the Verifier? (NeurIPS 2026 WS); VERSE (2610.02616) |
| D27 | Redirect classification needs recurrence thresholds | DSAT detection agreed with humans at κ = 0.50; 42% of invalid misalignment extractions were "normative prior bias"; users leave text feedback ~2× more when dissatisfied | WildFeedback (2408.15549); How Coding Agents Fail Their Users |
| D28 | Session outcome records with exact signature, MinHash, facets | Research-log of tested hypotheses prevents retesting; "stop at 6 queries" overfit (95% one batch, 15% next) | Adaptive Auto-Harness |
