# Troubleshooting

- **Nothing ever gets staged.** `rematch status --text`: are sessions traced (count > 0)? If not, the extension is not loaded (`rematch doctor`, `omp plugin doctor`). If traced but never flagged, your redirects are not being pre-labelled; use `/rematch mark correction` once to see the loop work, and check `library why` for `prelabel`.
- **It staged something but a later session did not load it.** `rematch match --cwd .` shows the match decision; `library why <session>` shows `glob hit · predicate miss` when the task did not resemble the original (different file types or no overlapping terms). That is by design: the entry only counts where it would have mattered.
- **The reflection chose KEEP_BASE.** The editor decided no rule would prevent that redirect (usually a scope change). `library why` shows the class; mark it `correction` if you disagree.
- **A session ran reflection with the wrong model.** The editor refuses to proceed if the provider/model of any reply differs from the configured role; the pending file is kept and retried next start (3 attempts). `doctor --text` shows the resolved roles.
- **omp is not on PATH for the binary** (ssh, cron): set `OMP_BIN=/path/to/omp`.
- **Profiles (`omp --profile <name>` / `OMP_PROFILE`).** The agent directory follows omp's profile resolution (`~/.omp/profiles/<name>/agent` when `OMP_PROFILE` or `PI_PROFILE` is set, else `$PI_CODING_AGENT_DIR`, else `~/.omp/agent`; `PI_CONFIG_DIR` replaces `~/.omp`), while `~/.rematch` stays shared unless `REMATCH_ROOT` is set. `import --history` imports the default agent dir and every profile's sessions except profiles listed in `--skip-profile <name>` or `~/.rematch/policy.json` (`{"skip_profiles":["…"]}`). `rematch doctor --text` shows the resolved `agent_dir` and which profiles exist or are skipped.

---

Source: moved from docs/MANUAL.md ("Troubleshooting").
