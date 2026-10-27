---
title: Lint rules
description: The static checks every staged entry passes, and the shipped lint.toml.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

Every staged entry passes lint. It is static and costs no model call; on an error the editor gets one retry with the findings. Why: [D15](/rematch/background/decisions/).

`improver/lint.toml` ships in the binary and is copied to `~/.rematch/improver/lint.toml` on first run. It is yours to edit; an upgrade refreshes it only while it still equals the previously shipped copy (DESIGN §1).

## Schema and rules

:::note[Generated]
From `docs/DESIGN.md` §12 (region `lint`) and `improver/lint.toml` by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

`improver/lint.toml` (copied to `REMATCH_ROOT/improver/` on first run). Every key is optional; a missing key takes the built-in value (`rematch_core::lint::DEFAULT_LINT_TOML`, which the shipped file equals verbatim — a test enforces it). All `[patterns]` entries are Rust `regex` syntax; use TOML multi-line literal strings (`'''…'''`) for patterns containing `'` (inside `'…'`, `''` is **not** an escape).

```toml
[limits]
always_on_bytes = 8192   # per branch, this entry + LintContext.branch_other_bytes: SKILL.md `description` values (omp lists every skill's description in each session's system prompt) and bodies (other frontmatter excluded), AGENT.md `description` values, AGENTS.fragment.md files (appended to the system prompt once approved)
max_stems = 10           # predicate.stems
max_exts = 8             # predicate.file_exts

[patterns]               # rule ids in brackets
session_id = [...]       # [identifiers] UUIDs (v4/v7), omp session file names
user_names = [...]       # [identifiers] home-dir paths that reveal a user; plus LintContext.user_names (literal, case-insensitive, word-bounded)
task_paths = [...]       # [identifiers] task-specific absolute paths (/Users, /home, /tmp, /var/folders)
urls = [...]             # [executable_risk]
network = [...]          # [executable_risk] curl, wget, nc, ssh, scp
pipe_shell = [...]       # [executable_risk] curl|wget … | sh
destructive_git = [...]  # [executable_risk] push -f/--force, reset --hard, clean -fd
string_match_user = [...] # [string_match_user] branching on the user's literal words
approval = [...]         # manifest.rationale matching any ⇒ executable_risk findings are Warn ("approval requested"), else Error
order_exception = [...]  # manifest.rationale matching any ⇒ form_order findings are Warn, else Error

[form]
order = ["trigger", "laws", "procedure", "pitfalls", "facts"]
[form.headings]          # lowercase substrings of a ##-heading that classify it; unclassified headings are ignored
trigger = ["trigger", "when to use", "scope"]
laws = ["laws", "rules", "invariants", "constraints"]
procedure = ["procedure", "workflow", "steps"]
pitfalls = ["pitfalls", "anti-patterns", "gotchas"]
facts = ["facts", "workspace facts", "reference"]

[hooks]
probes = ["echo ok", "cargo test", "git status", "README.md", "src/lib.rs"]  # a hook regex matching "" or every probe "matches everything"
```

Rules (`LintFinding.rule`): `identifiers`, `string_match_user`, `executable_risk` (applied to every file in the entry dir except `rematch.json` and `EXPERIENCE.md`; markdown is linted with its `rematch:` frontmatter block removed; `hook.json` contributes only `message` + `pass_condition`); `trigger` (non-empty repo glob, stem/ext caps, non-empty trigger line present in the SKILL.md body or any `Use when…` line, non-empty `description`; for `kind=agent` the document is `AGENT.md`: frontmatter `name` equal to the entry name and not `main`/`sub`, non-empty `description`, no trigger-line or form-order check on the system prompt); `scripts` (every `scripts/<path>` referenced in markdown exists; no script executable unless listed in `Entry.approved_scripts`); `always_on` (the `always_on_bytes` sum above: SKILL.md `description` + body, AGENT.md `description`, AGENTS.fragment.md); `form_order` (trigger line, then classified headings, non-decreasing in `form.order`); `facts_as_of` (`facts/*.md`, and SKILL.md of `kind=fact`, need frontmatter `as_of`); `hook` (`lint::hook_problems`, per mode: every mode a rule, a non-empty pass condition and a tool (`*` = any), no parameter of another mode; `block`: field, message, regex compiles and does not match everything; `stuck`: message, `repeats` in 2..=20; `truncate`: `max_age_turns` 1..=100, `keep_chars` 200..=100000; `retrieve`: `budget_bytes` 500..=65536, `min_overlap` 1..=20; non-block modes take no regex/field; `hook_module` refuses the same problems); `fragment` (`AGENTS.fragment.md` non-empty, no `#`/`##` heading outside a code fence: it is appended under `## rematch: <name>`); `fork_path` (`Entry.fork_path` repo-relative without `..`, `library::validate_fork_path`); `transfer` (`LintContext.transfer` and a domain lesson or fact). `LintReport.ok` iff no Error.

## The shipped `lint.toml`

```toml title="improver/lint.toml"
# Static lint rules for rematch proposals (docs/DESIGN.md §12).

[limits]
always_on_bytes = 8192
max_stems = 10
max_exts = 8

[patterns]
session_id = [
  '\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[47][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}\b',
  '\b\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}[0-9a-zA-Z._-]*_[0-9a-fA-F-]{36}\b',
]
user_names = [
  '''(?:^|[\s"'`(])/Users/[A-Za-z0-9._-]+''',
  '''(?:^|[\s"'`(])/home/[A-Za-z0-9._-]+''',
  '''(?:^|[\s"'`(])~/(?:repos|src|code|work|projects|dev|Desktop|Documents|Downloads)/''',
]
task_paths = [
  '''(?:^|[\s"'`(])/(?:Users|home|var/folders|tmp|private/tmp)/[^\s)"'`>]+''',
]
urls = [
  '''https?://[^\s)"'`>]+''',
]
network = [
  '\b(?:curl|wget)\b',
  '\b(?:nc|ssh|scp)\s+',
]
pipe_shell = [
  '\b(?:curl|wget)\b[^\n|]*\|\s*(?:ba|z|da|k)?sh\b',
]
destructive_git = [
  '\bgit\s+push\s+(?:[^\n]*\s+)?(?:-f|--force)\b',
  '\bgit\s+reset\s+--hard\b',
  '\bgit\s+clean\s+-[a-zA-Z]*[fd][a-zA-Z]*\b',
]
string_match_user = [
  '(?i)\bif\s+the\s+user\s+says\b',
  '(?i)\bwhen\s+the\s+user\s+types\b',
  '(?i)\buser\s+says\s+"',
]
approval = [
  '(?i)\b(?:approval_requested|requires?\s+approval|human\s+approval|one-tap\s+approval)\b',
]
order_exception = [
  '(?i)\b(?:form\s+order|section\s+order|order\s+exception)\b',
]

[form]
order = ["trigger", "laws", "procedure", "pitfalls", "facts"]

[form.headings]
trigger = ["trigger", "when to use", "scope"]
laws = ["laws", "rules", "invariants", "constraints"]
procedure = ["procedure", "workflow", "steps"]
pitfalls = ["pitfalls", "anti-patterns", "gotchas"]
facts = ["facts", "workspace facts", "reference"]

[hooks]
probes = ["echo ok", "cargo test", "git status", "README.md", "src/lib.rs"]
```
