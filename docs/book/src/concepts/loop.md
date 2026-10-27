# The loop

**Do nothing.** Work in omp as usual. Every session is traced. A session that ended with a redirect, a tool-error cluster, or a takeover produces one bounded proposal at the start of the next session, staged as `provisional`. Later sessions that match load it. Counters decide whether it stays.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../assets/loop-dark.svg">
    <img alt="The rematch loop. In the session: trace, flag, load, guard. Between sessions: classify, triage, propose, lint, stage, count and decide. An evidence boundary separates them: no proposal without a trace. You see a digest, approve or decline, mark, or turn it off." src="../assets/loop-light.svg" width="100%">
  </picture>
</p>

## How a reflection works

| Step | What happens | Enforced by |
|---|---|---|
| **Flag** | At shutdown the extension flags the session only if it carried a signal: a pre-labelled redirect (`interrupt`, `revert`, `negation`, `reprompt`), three tool errors in any six consecutive results, or `/rematch mark`. Clean sessions produce nothing. | extension; a 2-second budget, so it writes one small file |
| **Classify** | One batched call labels each redirect `correction`, `preference`, `scope` or `none`. A session whose only redirects are `scope` or `none` carries no signal and gets no editor call. A mark overrides the classifier and is never overridden. | binary (`reflect.rs`) |
| **Triage** | The first failure is either *process* (anything the agent produced with tools, tool-error clusters, takeovers, reverts, procedural text) or *content* (a corrected prose answer). Content failures are logged, not proposed on. Separately, the scope picks the editor: *domain* by default, *invariant* when the failure is about the agent's general way of working. | `trace::triage` |
| **Predicate** | Repo path, the extensions of files touched in the evidence window, and up to ten stems ranked by tf-idf over the task turn, the touched files and the redirect. The hash of this predicate keys the rate limits. | `predicate::derive` |
| **Propose** | One editor session through `omp -p --no-tools`, inputs in the prompt: the trace excerpt, the deliverable, the redirect, the current entry if any, matching `EXPERIENCE.md` lessons. The prompt treats the trace as data. The editor returns edits plus a manifest (evidence moment, predicted fix, predicted regression). `KEEP_BASE` is valid. There is no composer: with no probes, choosing between two untested texts would be blind selection. | `improver/editor.md`; `omp.rs` verifies provider and model on every event |
| **Lint** | Static, free, every rule in `improver/lint.toml`. On an error the editor gets one retry with the findings. | `lint::lint_entry` |
| **Stage** | Written to `staging/<branch>/<name>/` as `provisional` with the predicate, model family, originating session and manifest. Near-duplicates (simhash) route evidence to the existing entry instead. | `library::stage`, `dedup` |
| **Load** | At the next session start the extension has the binary link the entries matching the working directory into the shared skills directory, which it adds to omp's skill paths before discovery; `match` gives the entries and hooks the session counts. Every session is an on-session; there is no withheld arm. | extension factory; `library::sync_shared_skills`, `library::list_for_match` |
| **Count · decide** | For every finished trace: did the motivating redirect class recur while the entry was loaded; did a tool-error cluster, a takeover or a disable happen. Then the decision rule. After a keep or retire the reflector writes `EXPERIENCE.md` beside the entry; later proposals read matching ones. The editor prompt itself is never amended. | `counters::fold_session`, `counters::decide` |

## Highlights

- **No proposal without a trace (D1).** Every entry cites the moment (`session_id`, `seq`) it came from. Editing a harness without an execution behind it degraded performance in Harness² (73.4 → 63.2 on code). A description-only skill is a `draft` that never loads until you run a task with it or accept it unverified.
- **No reruns in interactive use (D6, D7).** The real session is the reference and the next matched session is the probe. A reflection spends at most three model calls: one batched classifier call, one editor, one lint retry. There is no composer without probes and no model selector anywhere: a composer choosing between untested texts is blind selection, which produced Harness²'s worst regressions.
- **Kept by use, retired by counters (D8, D10).** The primary signal is recurrence of the specific redirect class that motivated the entry, not tokens, not turns, not a judge. Counts are raw and shown raw. The status is called `kept`, never `verified`: unvalidated model-written patches were net negative in Harness-R1 (21 of 39 lowered success).
- **Nothing executable without you (D4).** Scripts are staged mode 0644. Hooks are compiled only after approval. Both arrive as one-tap approvals in the session-start digest, with the diff. Compiled hooks are the form that raised compliance with user corrections from 55.0% to 70.1% in Trace; they are also the form that can act without being read.
- **Lint is static and free (D15).** No session ids, task paths or user names in the text; no string-match on the user's words; a trigger predicate and a trigger line on every entry; no URLs, network calls or destructive git; always-on text within 8 KB per branch; facts carry `as_of`. HarnessCompass's generalization gate alone recovered most of its gain.
- **A bad entry cannot cascade (D13).** One admission per trigger predicate per day, ten provisional entries per branch, no new reflection while a canary for the same trigger is pending, caps that force merges, and an archive that is never deleted. Every unbounded library in the literature peaked and then declined.
- **Triggers stay in their domain.** Sessions and triggers carry a domain (`web.frontend`, `mobile`, `quant.options`): declared with `rematch policy domain set`, named in a branch's `domain.yml`, else inferred without a model from file extensions and words. A review skill learned on web work never loads or counts on mobile or 3D work, at most 8 entries are kept per domain, and `report` groups its triggers by domain. See [Domains](predicates-and-domains.md#domains).
- **The library is appended, never rewritten (D14).** Entries are itemized units with their own counters; a model never rewrites the library or the improver prompts (D25). Monolithic rewrites collapsed a context from 18,282 tokens to 122 in ACE, below baseline.
- **The schema lives in Rust only (D24).** The extension forwards raw events and parses nothing. One static binary owns the ledger, the library, lint, staging, batch mode, PR output. It never holds provider credentials; model calls go through `omp -p` and every event's provider and model are verified. The model family is recorded on every entry and never gated on (D20).
- **Rollback and uninstall (D13).** Every kept entry keeps its predecessor. `uninstall` removes every file the plugin wrote and nothing else.
- **Expectations are modest by design (D18, D19).** Published harness gains were 54% overfitting on audit; the deployed-static regime this tool runs in showed low single digits for strong models and more for weak ones. The project has a kill criterion (see [Status and limits](../status.md)) and no gain has been measured on real use yet.
- **Predicates learn from reads, under the same counters (D12, D15).** A skill the model opened in three sessions its predicate missed gets the stems those sessions share; a predicate that matched four sessions where the skill was loaded and never read loses the stems only those carry. No model call, and never an edit in place: `rematch tune` (and every `count`) stages a provisional sibling with the new predicate, a new canary that the counters keep or retire, with its evidence session ids in `rematch.json`. Guards: one tune per entry per 7 days, never on pinned, built-in or hook entries, never a new repo, remote or `**`, never another domain, ≤ 10 stems, ≤ 8 extensions, ≥ 2 stems after narrowing, lint and rate limits unchanged; `report` shows both versions' with-arms so a wider predicate cannot win the verdict by matching more.

## Models

Every entry records the model family it was created under and keeps a per-family breakdown of its counters. Entries load regardless of family; gating would make them flicker as you switch models. When the counters diverge, harm on one family and clean on another, the entry is split and the harmed family gets its own sibling. A family whose recurrence or harm rises across several entries in one window appears as a review list in the digest, never as automatic retirements. The editor is the strongest configured model; the classifier a fast one; a different provider than configured is a failed run, never a fallback. There is no judge role.

---

Source: README.md ("What you can do with it", "How it works", "How a reflection works", "Highlights", "Models"); the step contracts are docs/DESIGN.md §3 (flag rule), §6 (predicate, triage), §9 (editor channel), §7 (counting).
