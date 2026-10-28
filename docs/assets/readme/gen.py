#!/usr/bin/env python3
"""Generates the README diagrams (light + dark) so both themes stay identical in content.

    python3 docs/assets/readme/gen.py
"""
from pathlib import Path

OUT = Path(__file__).parent
FONT = "system-ui, -apple-system, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"

THEMES = {
    "light": dict(bg="#f7f7f7", panel="#ffffff", line="#e5e5e5", ink="#171717", mid="#666666",
                  dim="#767676", accent="#4f46e5", accent_ink="#4338ca", ok="#15803d", warn="#b45309",
                  bad="#b91c1c", term_bg="#171717", term_ink="#f2f2f2", term_dim="#9a9a9a"),
    "dark": dict(bg="#0a0a0a", panel="#121212", line="#262626", ink="#f2f2f2", mid="#9a9a9a",
                 dim="#6b6b6b", accent="#6360f0", accent_ink="#a9abff", ok="#4ade80", warn="#fbbf24",
                 bad="#f87171", term_bg="#121212", term_ink="#f2f2f2", term_dim="#9a9a9a"),
}


def esc(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def head(w, h, title, desc, t):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" '
        f'aria-labelledby="title desc">\n<title id="title">{esc(title)}</title>\n<desc id="desc">{esc(desc)}</desc>\n'
        f'<defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" '
        f'orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" fill="{t["mid"]}"/></marker>'
        f'<marker id="arrow-accent" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" '
        f'orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" fill="{t["accent"]}"/></marker></defs>\n'
        f'<g font-family="{FONT}">\n<rect x="0.5" y="0.5" width="{w-1}" height="{h-1}" rx="16" fill="{t["bg"]}" stroke="{t["line"]}"/>\n'
    )


TAIL = "</g></svg>\n"


def text(x, y, s, size=14, fill=None, weight=None, anchor=None, spacing=None, family=None):
    a = f' text-anchor="{anchor}"' if anchor else ""
    wgt = f' font-weight="{weight}"' if weight else ""
    sp = f' letter-spacing="{spacing}"' if spacing else ""
    fam = f' font-family="{family}"' if family else ""
    return f'<text x="{x}" y="{y}" font-size="{size}"{wgt}{sp}{a}{fam} fill="{fill}">{esc(s)}</text>\n'


def panel(x, y, w, h, t, title, sub, rows, t_ink=None):
    """A titled card with rows of (name, note)."""
    s = f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="{t["panel"]}" stroke="{t["line"]}"/>\n'
    s += text(x + 22, y + 36, title, 12, t["mid"], 600, spacing="0.12em")
    s += text(x + 22, y + 59, sub, 13, t["dim"])
    s += f'<line x1="{x+22}" y1="{y+78}" x2="{x+w-22}" y2="{y+78}" stroke="{t["line"]}"/>\n'
    yy = y + 78
    n = len(rows)
    avail = h - 78 - 16
    step = avail / n
    for i, (name, note) in enumerate(rows):
        cy = yy + step * i
        s += text(x + 22, cy + step * 0.44, name, 16, t_ink or t["ink"], 600)
        s += text(x + 22, cy + step * 0.44 + 19, note, 12.5, t["mid"])
        if i < n - 1:
            s += f'<line x1="{x+22}" y1="{cy+step}" x2="{x+w-22}" y2="{cy+step}" stroke="{t["line"]}"/>\n'
    return s


def wordmark(theme):
    t = THEMES[theme]
    ink = t["ink"]
    paper = t["bg"] if theme == "light" else "#0a0a0a"
    s = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 420 80" width="420" height="80" role="img" aria-labelledby="title">\n'
        f'<title id="title">rematch</title>\n'
        f'<rect x="4" y="8" width="64" height="64" rx="12" fill="{ink}"/>\n'
        # a loop that returns on itself: evidence → entry → evidence
        f'<path d="M36 22 A18 18 0 1 1 18 40" fill="none" stroke="{paper}" stroke-width="5" stroke-linecap="round"/>\n'
        f'<path d="M12 33 L18 42 L26 35" fill="none" stroke="{paper}" stroke-width="5" stroke-linecap="round" stroke-linejoin="round"/>\n'
        f'<circle cx="36" cy="40" r="5" fill="{paper}"/>\n'
        f'<text x="86" y="58" font-family="{FONT}" font-size="48" font-weight="600" letter-spacing="-0.01em" fill="{ink}">rematch</text>\n'
        f'</svg>\n'
    )
    return s


def loop(theme):
    t = THEMES[theme]
    w, h = 1100, 640
    s = head(w, h, "The rematch loop",
             "The adapter records session evidence, exposes scoped trials and runs approved policies. A detached "
             "worker classifies failures, proposes and lints changes, then observes actual use or runs an authorized "
             "bounded reference/candidate comparison. The engine retains accepted-in-use or comparatively supported "
             "changes, and withdraws harmful ones. Personal instruction changes and executable policies have a "
             "persistent review action; team sharing is optional.", t)
    s += panel(30, 56, 420, 420, t, "IN THE SESSION", "extension · index.ts · no model calls", [
        ("trace", "every input, tool result, turn, model change → traces/"),
        ("flag", "redirect, tool-error cluster, takeover, /rematch mark → pending/"),
        ("load", "scoped skill and agent views; catalog and body budgets differ"),
        ("guard", "approved hooks run on tool_call; block with the rule's message"),
    ])
    s += panel(650, 56, 420, 420, t, "BACKGROUND WORKER", "rematch · Rust · bounded execution", [
        ("classify · triage", "the failure, its scope and the originating moment"),
        ("propose", "one scoped editor; KEEP_BASE remains valid"),
        ("lint · budget", "safe metadata, scoped exposure, separate body budget"),
        ("trial · review", "automatic low-risk trials; persistent approval actions"),
        ("observe · probe", "relevant use, or authorized reference/candidate runs"),
        ("decide", "accepted in use · comparative support · withdraw"),
    ])
    s += text(240, 506, "runs the model's session · writes only the trace", 13, t["dim"], anchor="middle")
    s += text(860, 506, "detached work · bounded probes only when replay is authorized", 13, t["dim"], anchor="middle")
    # boundary
    s += f'<line x1="550" y1="50" x2="550" y2="486" stroke="{t["accent"]}" stroke-width="2" stroke-dasharray="6 6"/>\n'
    s += f'<rect x="458" y="24" width="184" height="24" rx="12" fill="{t["bg"]}"/>\n'
    s += text(550, 41, "EVIDENCE BOUNDARY", 12, t["accent_ink"], 600, "middle", "0.12em")
    s += text(550, 506, "no proposal without a trace", 13, t["accent_ink"], anchor="middle")
    # arrows across
    s += f'<rect x="462" y="160" width="176" height="38" fill="{t["bg"]}"/>\n'
    s += text(550, 176, "traces, pending flags,", 13, t["mid"], anchor="middle")
    s += text(550, 193, "marks", 13, t["mid"], anchor="middle")
    s += f'<line x1="458" y1="210" x2="640" y2="210" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += f'<line x1="642" y1="300" x2="460" y2="300" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += f'<rect x="458" y="312" width="184" height="38" fill="{t["bg"]}"/>\n'
    s += text(550, 328, "staging dirs, hook modules,", 13, t["mid"], anchor="middle")
    s += text(550, 345, "the digest", 13, t["mid"], anchor="middle")
    # you
    s += f'<rect x="400" y="546" width="300" height="66" rx="12" fill="{t["panel"]}" stroke="{t["mid"]}"/>\n'
    s += text(550, 574, "You", 17, t["ink"], 600, "middle")
    s += text(550, 596, "/rematch review · /rematch mark · /rematch off", 13, t["mid"], anchor="middle")
    s += f'<path d="M370 478 C370 540 384 579 394 579" fill="none" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += text(360, 552, "digest", 13, t["mid"], anchor="end")
    s += f'<path d="M706 579 C716 579 730 540 730 482" fill="none" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += text(740, 552, "approve · decline · disable", 13, t["mid"])
    return s + TAIL


def architecture(theme):
    t = THEMES[theme]
    w, h = 1100, 600
    s = head(w, h, "rematch architecture",
             "omp emits extension events and discovers scoped skill and agent views. The binary owns the ledger, "
             "library, lint, adoption actions and bounded probes, and calls omp for editor and solver work. "
             "Personal AGENTS.md changes require approval and preserve user text. Canonical state lives under "
             "~/.rematch; sharing to teams or forks is optional.", t)
    s += panel(30, 56, 230, 420, t, "OMP", "stock 18.12, or your fork", [
        ("events", "start · input · tool · turn · shutdown"),
        ("skills", "customDirectories, set before discovery"),
        ("omp -p", "JSON event stream, model verified"),
        ("plugin", "omp plugin link extension/"),
    ])
    s += panel(300, 56, 230, 420, t, "EXTENSION", "index.ts · forwards raw events", [
        ("trace writer", "append-only JSONL, bounded fields"),
        ("prelabels", "interrupt · revert · negation · reprompt"),
        ("match → load", "pi.exec match --cwd at factory time"),
        ("tool_call hooks", "rematch-hooks/<id>.ts, approved only"),
        ("rematch · /rematch", "long work returns a job id"),
    ])
    s += panel(570, 56, 270, 420, t, "BINARY", "rematch · Rust · never holds credentials", [
        ("rematch-core", "trace · predicate · lint · library · ledger"),
        ("reflect · count", "propose → trial → use evidence → decide"),
        ("personal adoption", "review → approved AGENTS.md section"),
        ("probes · batch", "worktrees, bounded runs, fixed verifier"),
        ("optional sharing", "ship · export · stale · split · transfer"),
    ])
    s += panel(880, 56, 190, 420, t, "STATE", "~/.rematch", [
        ("traces/", "the only live input"),
        ("ledger.sqlite", "sessions · entries · counters"),
        ("staging/<branch>/", "canonical trial content"),
        ("archive/ · drafts/", "never loaded"),
        ("~/.omp/agent/skills/", "kept entries"),
    ])
    # arrows between columns
    s += f'<line x1="262" y1="150" x2="298" y2="150" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += f'<line x1="298" y1="234" x2="262" y2="234" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += f'<line x1="532" y1="300" x2="568" y2="300" stroke="{t["accent"]}" stroke-width="1.5" marker-end="url(#arrow-accent)"/>\n'
    s += f'<line x1="568" y1="320" x2="532" y2="320" stroke="{t["accent"]}" stroke-width="1.5" marker-end="url(#arrow-accent)"/>\n'
    s += f'<line x1="842" y1="266" x2="878" y2="266" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += f'<line x1="878" y1="286" x2="842" y2="286" stroke="{t["mid"]}" stroke-width="1.5" marker-end="url(#arrow)"/>\n'
    s += text(280, 140, "events", 11, t["dim"], anchor="middle")
    s += text(280, 250, "skill paths", 11, t["dim"], anchor="middle")
    s += text(550, 290, "argv", 11, t["accent_ink"], anchor="middle")
    s += text(550, 336, "envelope", 11, t["accent_ink"], anchor="middle")
    # omp -p loop back from the binary to omp, routed under the cards
    s += f'<path d="M705 478 C705 526 150 526 145 480" fill="none" stroke="{t["accent"]}" stroke-width="1.5" stroke-dasharray="6 6" marker-end="url(#arrow-accent)"/>\n'
    s += text(425, 540, "omp -p --no-tools --mode json: editor, classifier, reflector", 12.5, t["accent_ink"], anchor="middle")
    s += text(425, 557, "omp -p with tools: batch rollouts in a fresh worktree", 12.5, t["accent_ink"], anchor="middle")
    s += text(550, 585, "JSON envelope on stdout: {ok, operation, data | error} · the schema lives in Rust only", 13, t["dim"], anchor="middle")
    return s + TAIL


def lifecycle(theme):
    t = THEMES[theme]
    w, h = 1100, 430
    s = head(w, h, "Entry statuses",
             "Draft becomes provisional when you give it a task or accept it unverified. Provisional becomes kept "
             "after sustained relevant clean use or engine-recorded comparative support, not from catalog presence "
             "alone. Repeated recurrence or harm retires an entry; inactivity may make it dormant. Kept entries "
             "remain reversible. Stale entries need a new canary; pinned entries remain user-owned.", t)

    def box(x, y, label, note, accent=False):
        stroke = t["accent"] if accent else t["line"]
        o = f'<rect x="{x}" y="{y}" width="170" height="64" rx="12" fill="{t["panel"]}" stroke="{stroke}" stroke-width="{2 if accent else 1}"/>\n'
        o += text(x + 85, y + 28, label, 17, t["ink"], 600, "middle")
        o += text(x + 85, y + 48, note, 12, t["mid"], anchor="middle")
        return o

    def arrow(d, color=None):
        c = color or t["mid"]
        m = "url(#arrow-accent)" if color == t["accent"] else "url(#arrow)"
        return f'<path d="{d}" fill="none" stroke="{c}" stroke-width="1.5" marker-end="{m}"/>\n'

    def label(x, y, s_, anchor="middle", color=None):
        return text(x, y, s_, 12, color or t["mid"], anchor=anchor)

    # row 2
    s += box(40, 150, "draft", "visible, never loaded")
    s += box(300, 150, "provisional", "scoped trial", accent=True)
    s += box(580, 150, "kept", "retained with evidence")
    s += box(860, 150, "stale", "excluded; evidence drifted")
    # row 3
    s += box(130, 308, "dormant", "excluded, kept on disk")
    s += box(720, 308, "retired", "archived with evidence")

    s += arrow("M210 182 L298 182") + label(255, 140, "task run, or --unverified")
    s += arrow("M470 182 L578 182", t["accent"]) + label(524, 124, "sustained relevant clean use", color=t["accent_ink"])
    s += label(524, 141, "or bounded comparative support")
    s += arrow("M750 182 L858 182") + label(804, 140, "as_of · family · fork commit changed")
    s += arrow("M945 150 C945 70 360 70 360 148") + label(652, 84, "new canary → provisional, counters from zero")
    s += arrow("M340 214 L270 308") + label(296, 262, "12 matched, no decision · 30 d, < 3 matches", anchor="end")
    s += arrow("M300 335 C390 335 400 300 400 216") + label(410, 300, "predicate hits again", anchor="start")
    s += arrow("M470 200 C540 200 540 350 718 350") + label(600, 372, "2 recurrences · 2 harm")
    s += arrow("M700 214 L775 308") + label(792, 250, "2 harm in 30 d · disable", anchor="start")
    s += arrow("M980 214 C980 345 950 345 892 345") + label(992, 292, "or retired", anchor="start")
    s += label(805, 395, "never leaves", color=t["dim"])
    s += text(40, 408, "pinned", 13, t["ink"], 600)
    s += label(96, 408, "a flag you set and unset; automatic transitions never touch a pinned entry", anchor="start")
    return s + TAIL


def terminal(theme, name, title, lines, h):
    """A terminal card. lines: list of (kind, text) with kind in prompt|out|dim|ok."""
    t = THEMES[theme]
    w = 1100
    s = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" aria-labelledby="title">\n'
         f'<title id="title">{esc(title)}</title>\n'
         f'<rect x="0.5" y="0.5" width="{w-1}" height="{h-1}" rx="16" fill="{t["term_bg"]}" stroke="{t["line"]}"/>\n'
         f'<circle cx="26" cy="24" r="6" fill="#ff5f57"/><circle cx="46" cy="24" r="6" fill="#febc2e"/><circle cx="66" cy="24" r="6" fill="#28c840"/>\n'
         f'<text x="550" y="29" text-anchor="middle" font-family="{FONT}" font-size="13" fill="{t["term_dim"]}">{esc(title)}</text>\n'
         f'<g font-family="{MONO}" font-size="13.5">\n')
    y = 64
    colors = {"prompt": t["term_ink"], "out": t["term_ink"], "dim": t["term_dim"], "ok": "#4ade80", "warn": "#fbbf24", "bad": "#f87171", "accent": "#a9abff"}
    for kind, line in lines:
        if kind == "prompt":
            s += f'<text x="28" y="{y}" fill="{t["term_dim"]}">$</text><text x="46" y="{y}" fill="{colors[kind]}">{esc(line)}</text>\n'
        elif kind == "gap":
            y += 8
            continue
        else:
            s += f'<text x="28" y="{y}" fill="{colors[kind]}" xml:space="preserve">{esc(line)}</text>\n'
        y += 22
    return s + "</g></svg>\n"


REFLECT_LINES = [
    ("prompt", "rematch reflect --pending --text"),
    ("dim", "counted 1 session(s), 0 counter row(s)"),
    ("out", "session 01a1760d: classifier → correction (0.86) · triage process/domain · moment seq 10"),
    ("ok", "staged src-lib-public-functions (skill) from session 01a1760d · lint ok · 1 model call"),
    ("gap", ""),
    ("prompt", "cat ~/.rematch/staging/rematch-e2e-repo/src-lib-public-functions/SKILL.md"),
    ("dim", "---"),
    ("dim", "name: src-lib-public-functions"),
    ("dim", "description: Use when adding or modifying public functions in src/lib.py."),
    ("dim", "rematch: {status: provisional, branch: rematch-e2e-repo, family: github-copilot/gemini-argon, version: 1}"),
    ("dim", "---"),
    ("out", "## Laws"),
    ("out", "- Export every public function name in `__all__` at the top of `src/lib.py`."),
    ("out", "- Include a Google-style docstring with `Args:` and `Returns:` on every public function."),
    ("out", "- Verify both `__all__` membership and docstring sections via Python assertions before yielding."),
]

WHY_LINES = [
    ("prompt", "rematch library why 01a17614-a543-77f3-8ad1-d28bbdc2c2fe --text"),
    ("out", "session 01a17614 · github-copilot/gemini-4-argon · 4 turns · 4 tool calls (0 errors, 0 clusters) · 0 takeovers"),
    ("ok", "loaded src-lib-public-functions (provisional): glob hit · predicate hit · this session recurrence 0 harm 0"),
    ("accent", "  total matched 1 · used 1 (1 clean) · recurrence 0 · harm 0"),
    ("gap", ""),
    ("prompt", "rematch library ls --text"),
    ("out", "provisional skill  src-lib-public-functions [rematch-e2e-repo] matched 1 · used 1 (1 clean) · recurrence 0 · harm 0"),
    ("gap", ""),
    ("prompt", "rematch digest --cwd . --text"),
    ("out", "staged skill src-lib-public-functions in rematch-e2e-repo: Use when adding or modifying public functions in src/lib.py."),
    ("dim", "approvals: none pending"),
]

PLUGIN_UI_LINES = [
    ("dim", "╭─── ✎ Write: src/lib.rs · 4 lines ─────────────────────────────────────────────────────────────────────╮"),
    ("out", "│   1 pub fn reconcile() -> String { \"1\".to_string() }                                                 │"),
    ("out", "│   3 pub fn total() -> u32 { reconcile() + 1 }                                                        │"),
    ("dim", "╰───────────────────────────────────────────────────────────────────────────────────────────────────────╯"),
    ("accent", "╭─ ◆ rematch · fast-check ───────────────────────────────────────────────────────────────── cargo · 0.1s ─╮"),
    ("bad", "│ ✗ 2 errors → sent to the agent                                                                        │"),
    ("out", "│   src/lib.rs:3:25  E0308  mismatched types (expected `u32`, found `String`)                            │"),
    ("warn", "│   ↳ 1 caller in unedited files: src/api.rs:12 total()                                                 │"),
    ("accent", "╰───────────────────────────────────────────────────────────────────────────────────────────────────────╯"),
    ("ok", "◆ rematch · ✓ cargo clean in 0.1s"),
    ("gap", ""),
    ("accent", "╭─ ◆ rematch · review ─────────────────────────────────────────────────────── [Needs you 1] Applied 4 ─╮"),
    ("warn", "│ ❯ ● ledger-snapshot   script · payments                                                               │"),
    ("out", "│   Make scripts/snapshot.sh executable · Why it waits: wrote outside its sandbox                       │"),
    ("dim", "│   Observed: writes ~/.ledger/snapshot.csv · no network · exit 0                                       │"),
    ("ok", "│   + cp ledger.csv \"$HOME/.ledger/snapshot.csv\"                                                       │"),
    ("dim", "│   a approve   d decline   i full diff   ↑↓ item   tab applied   esc later                             │"),
    ("accent", "╰───────────────────────────────────────────────────────────────────────────────────────────────────────╯"),
    ("warn", "◆ rematch  ● 1 needs you: ledger-snapshot (script writes ~/.ledger)   alt+e review"),
    ("accent", "◆ rematch  ✓ cargo 0.1s  ·  5 on trial"),
]

EVAL_LINES = [
    ("prompt", "rematch create ux-review --from description --description \"Review UI/TUI screens for contrast, hierarchy and states\" \\"),
    ("out", "  --eval-prompt \"Audit this checkout screen and report P0/P1 layout or contrast issues\""),
    ("ok", "drafted ux-review at ~/.rematch/drafts/ux-review · seeded evals/evals.json (1 case, 2 trigger queries)"),
    ("gap", ""),
    ("prompt", "rematch eval trigger ux-review --text"),
    ("ok", "trigger pass 2/2 (precision 1.00, recall 1.00) · retrieved units/tui-layout.md · catalog 118/1024 B · router 1420/8192 B"),
    ("gap", ""),
    ("prompt", "rematch eval run ux-review --activate --text"),
    ("ok", "case-1: win — with_skill passed verifier (score 1.0, read ux-review) vs baseline fail · 0 regressions"),
    ("accent", "activated ux-review as provisional (passed validation gate) · report: ~/.rematch/evals/ux-review/report.html"),
]


def main():
    for theme in THEMES:
        (OUT / f"wordmark-{theme}.svg").write_text(wordmark(theme))
        (OUT / f"loop-{theme}.svg").write_text(loop(theme))
        (OUT / f"architecture-{theme}.svg").write_text(architecture(theme))
        (OUT / f"lifecycle-{theme}.svg").write_text(lifecycle(theme))
        (OUT / f"reflect-{theme}.svg").write_text(terminal(theme, "reflect", "a reflection, from a real session (gemini-4-argon editor)", REFLECT_LINES, 420))
        (OUT / f"why-{theme}.svg").write_text(terminal(theme, "why", "the next matched session, counted", WHY_LINES, 320))
        (OUT / f"plugin-ui-{theme}.svg").write_text(terminal(theme, "plugin-ui", "omp / Pi TUI: ◆ rematch fast-check card, review overlay (alt+e), widget and footer", PLUGIN_UI_LINES, 480))
        (OUT / f"eval-{theme}.svg").write_text(terminal(theme, "eval", "skill creator workbench: draft, trigger check, side-by-side eval run, validation gate", EVAL_LINES, 300))


if __name__ == "__main__":
    main()
