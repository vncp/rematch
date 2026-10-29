// Behavior of the shared rendering module: the review panel's keys (a pure state machine), the
// output line classifier, and that no renderer returns a line wider than asked.
import { describe, expect, test } from "bun:test";
import type { AutoApplied, ReviewAction } from "./index.ts";
import {
	box,
	checkCardLines,
	classifyLine,
	commandCompletions,
	digestCardLines,
	digestRows,
	footerStatus,
	keyOf,
	needsYouLine,
	outputLines,
	panelEmpty,
	panelKey,
	panelResult,
	reviewPanel,
	reviewPanelLines,
	summarizeEnvelope,
	truncate,
	type UiTheme,
	visibleWidth,
} from "./ui.ts";

const action = (over: Partial<ReviewAction> = {}): ReviewAction => ({
	id: "01a180e9-1fcc-76c6-afee-2e7dc1206806",
	kind: "scripts",
	name: "ledger-snapshot",
	summary: "make 1 script(s) executable of script ledger-snapshot",
	state: "pending",
	blocked: null,
	reload: null,
	approve: ["library", "approve", "01a180e9-1fcc-76c6-afee-2e7dc1206806", "--sha", "a".repeat(64)],
	decline: ["library", "decline", "01a180e9-1fcc-76c6-afee-2e7dc1206806", "--sha", "a".repeat(64)],
	needsYou: true,
	place: "payments",
	reason: "seq 1",
	diff: `+++ scripts/snapshot.sh (sha256 ${"b".repeat(64)})\n#!/bin/sh\nmkdir -p "$HOME/.ledger"\n--- sandbox dry run\nscripts/snapshot.sh: mkdir: ~/.ledger: Operation not permitted\n`,
	observed: { runner: "sandbox-exec", clean: false, exit_code: 1, writes_outside: ["~/.ledger"], network: false },
	...over,
});
const applied = (over: Partial<AutoApplied> = {}): AutoApplied => ({
	id: "01a180e9-1f9c-77b5-95be-2cc38059f90a",
	name: "fast-check",
	kind: "hook",
	what: "hook fast-check (check)",
	why: "data-only",
	rule: null,
	undo_argv: ["library", "disable", "fast-check"],
	...over,
});

/** A theme that wraps every token in SGR codes, so widths must ignore escapes. */
const COLOR: UiTheme = { fg: (_t, s) => `\x1b[38;5;3m${s}\x1b[39m`, bold: s => `\x1b[1m${s}\x1b[22m` };

describe("review panel keys", () => {
	test("a approves the action under the cursor with its own bound argv; success removes it and says so", () => {
		const s0 = reviewPanel([action(), action({ id: "x", name: "other" })], [applied()]);
		const down = panelKey(s0, "down").state;
		expect(down.cursor.needs).toBe(1);
		const { state, effect } = panelKey(s0, "a");
		expect(effect).toEqual({ type: "approve", action: s0.needs[0]!, argv: s0.needs[0]!.approve! });
		expect(state.busy).toBeTruthy();
		expect(panelKey(state, "d").effect, "keys wait while it runs").toBeNull();
		const done = panelResult(state, effect!, true);
		expect(done.needs.map(a => a.name)).toEqual(["other"]);
		expect(done.notice).toEqual({ tone: "success", text: "✓ approved ledger-snapshot" });
	});

	test("a failure keeps the item and shows the error inline", () => {
		const s0 = reviewPanel([action()], []);
		const { state, effect } = panelKey(s0, "d");
		expect(effect?.type).toBe("decline");
		const failed = panelResult(state, effect!, false, "sha mismatch");
		expect(failed.needs).toHaveLength(1);
		expect(failed.notice?.tone).toBe("error");
		expect(failed.busy).toBeNull();
	});

	test("a blocked action cannot be approved, only declined", () => {
		const s0 = reviewPanel([action({ blocked: "needs a verifier", needsYou: false })], []);
		expect(panelKey(s0, "a").effect).toBeNull();
		expect(panelKey(s0, "a").state.notice?.tone).toBe("muted");
		expect(panelKey(s0, "d").effect?.type).toBe("decline");
	});

	test("undo asks first: u, then y runs the undo argv; n or esc cancels", () => {
		const s0 = panelKey(reviewPanel([action()], [applied()]), "tab").state;
		expect(s0.tab).toBe("applied");
		const asking = panelKey(s0, "u").state;
		expect(asking.confirmUndo).toBe(applied().id);
		expect(panelKey(asking, "escape")).toEqual({ state: { ...asking, confirmUndo: null }, effect: null });
		expect(panelKey(asking, "n").state.confirmUndo).toBeNull();
		const { effect } = panelKey(asking, "y");
		expect(effect).toEqual({ type: "undo", item: applied(), argv: ["library", "disable", "fast-check"] });
	});

	test("a change nothing undoes is not offered", () => {
		const s0 = reviewPanel([], [applied({ undo_argv: null, why: "probe_regressed" })]);
		expect(s0.tab).toBe("applied");
		expect(panelKey(s0, "u").state.confirmUndo).toBeNull();
	});

	test("esc closes; the panel is empty when both lists are", () => {
		const s0 = reviewPanel([action()], []);
		expect(panelKey(s0, "escape").effect).toEqual({ type: "close" });
		const { state, effect } = panelKey(s0, "a");
		expect(panelEmpty(panelResult(state, effect!, true))).toBe(true);
	});

	test("raw terminal input maps to keys, Kitty protocol included", () => {
		expect([keyOf("\x1b"), keyOf("\x1b[27u"), keyOf("\x1b[A"), keyOf("\x1b[1;1B"), keyOf("\t"), keyOf("\x1b[97u"), keyOf("a")]).toEqual([
			"escape", "escape", "up", "down", "tab", "a", "a",
		]);
	});
});

describe("line classification", () => {
	test("the binary's line prefixes pick the class", () => {
		expect(classifyLine("approval pending: make 1 script(s) executable")).toBe("needs");
		expect(classifyLine("blocked probe: needs a verifier")).toBe("blocked");
		expect(classifyLine("applied automatically: hook fast-check (check) (data-only)")).toBe("applied");
		expect(classifyLine("kept skill crate-tests-first (matched 4)")).toBe("kept");
		expect(classifyLine("retired skill old-helper (2 harms)")).toBe("retired");
		expect(classifyLine("provisional hook   fast-check [shared]")).toBe("trial");
		expect(classifyLine("  help: try this")).toBe("detail");
		expect(classifyLine("something else")).toBe("plain");
		expect(classifyLine("win case-1: verifier: baseline fail → with-skill pass")).toBe("ok");
		expect(classifyLine("pass should trigger, \"review the form\": triggered")).toBe("ok");
		expect(classifyLine("regression case-2: verifier: baseline pass → with-skill fail")).toBe("error");
		expect(classifyLine("fail should not trigger, \"write a poem\": triggered")).toBe("error");
		expect(classifyLine("refused --activate: 1 regression(s) vs baseline")).toBe("error");
		expect(classifyLine("activated ux-review as provisional: 1 win(s), no regression")).toBe("trial");
		expect(classifyLine("tie case-3: no verifier verdict")).toBe("plain");
	});

	test("eval results summarize as one line", () => {
		expect(summarizeEnvelope({ ok: true, data: { summary: { wins: 2, ties: 1, regressions: 0 }, activated: true, lines: ["a", "b"] } }).text).toBe("2 win · 1 tie · 0 regression · activated");
		expect(summarizeEnvelope({ ok: true, data: { queries: [{ pass: true }, { pass: false }], pass_rate: 0.5, lines: ["a", "b"] } }).text).toBe("trigger pass 1/2");
	});
});

describe("width safety", () => {
	const widths = [120, 80, 40, 12, 7, 1];
	const rendered = "[rematch fast-check] cargo: 2 error(s) in 0.4s\nsrc/lib.rs:3:25 [E0369] cannot add `{integer}` to `String` and this message is long enough to need cutting at narrow widths\n  help: try `.parse()`\nimpact (references in unedited files):\n  src/api.rs:12 (total): total()";
	const digest = { since: 0, changed: [{ id: "k", name: "crate-tests-first", status: "kept", counters: { used_clean: 4 } }], applied_automatically: [], probes: [] };
	const surfaces: [string, (w: number) => string[]][] = [
		["check card", w => checkCardLines(COLOR, w, rendered, { kind: "cargo", ok: false, errors: 2, duration_ms: 400 }, false)],
		["check card expanded", w => checkCardLines(COLOR, w, rendered, { kind: "cargo", ok: false, errors: 2, duration_ms: 400 }, true)],
		["clean check", w => checkCardLines(COLOR, w, "✓ cargo check passed in 0.4s", { kind: "cargo", ok: true, errors: 0, duration_ms: 400 }, false)],
		["digest card", w => digestCardLines(COLOR, w, digestRows(digest, [action()], "alt+e", ["fast-check", "stuck-detector"]).rows, false)],
		["review panel", w => reviewPanelLines(COLOR, w, reviewPanel([action()], [applied()]))],
		["applied tab", w => reviewPanelLines(COLOR, w, panelKey(reviewPanel([action()], [applied()]), "tab").state)],
		["output table", w => outputLines(COLOR, w, { ok: true, command: "library ls", data: { entries: [{ name: "fast-check", kind: "hook", status: "provisional", branch: "shared", counters: { used: 1, harm: 0 } }], actions: [] } }, "", false)],
		["output lines", w => outputLines(COLOR, w, { ok: true, command: "why", data: {} }, "approval pending: x\n  help: y\nkept skill z", false)],
		["eval output", w => outputLines(COLOR, w, { ok: true, command: "eval run ux-review", data: { summary: { wins: 1, ties: 0, regressions: 0 } } }, "eval run ux-review: 1 case(s) · wins 1\nwin case-1: verifier: baseline fail → with-skill pass · skill read\n  gate passed: 1 win(s), no regression\nactivated ux-review as provisional", false)],
		["output error", w => outputLines(COLOR, w, { ok: false, command: "doctor", error: "rematch binary not found" }, "", false)],
		["box", w => box(COLOR, w, { title: "◆ rematch", right: "since last session", body: ["a".repeat(200)] })],
		["widget", w => [truncate(needsYouLine(COLOR, [action()], "alt+e"), w)]],
	];
	for (const [name, render] of surfaces) {
		test(`${name}: no line wider than the width`, () => {
			for (const w of widths) for (const line of render(w)) expect(visibleWidth(line), `${name} at ${w}: ${JSON.stringify(line)}`).toBeLessThanOrEqual(w);
		});
	}

	test("collapsed views show no ids or hashes", () => {
		const text = reviewPanelLines(COLOR, 120, reviewPanel([action()], [applied()])).join("\n");
		expect(text).not.toMatch(/[0-9a-f]{8}-[0-9a-f]{4}-/);
		expect(text).not.toMatch(/[0-9a-f]{40}/);
		expect(text).toContain("ledger-snapshot");
	});

	test("the footer names the check, trials and what needs you; nothing to say is no status", () => {
		const s = footerStatus({ fg: (_t, x) => x }, { check: { state: "done", kind: "cargo", ok: false, errors: 2, durationMs: 400, timedOut: false, impact: 0 }, trials: 6, needsYou: 1, reviewKey: "alt+e" });
		expect(s).toBe("◆ rematch ✗ cargo 2 errors · 6 on trial · ● 1 needs you alt+e");
		expect(footerStatus({ fg: (_t, x) => x }, { check: null, trials: 0, needsYou: 0, reviewKey: null })).toBeUndefined();
	});
});

describe("completions", () => {
	test("subcommands, then names of what waits", () => {
		expect(commandCompletions("ap", ["approve", "activate", "why"], () => [])?.map(c => c.value)).toEqual(["approve "]);
		expect(commandCompletions("approve led", ["approve"], () => [{ name: "ledger-snapshot", description: "" }])?.map(c => c.value)).toEqual(["approve ledger-snapshot"]);
		expect(commandCompletions("why x", ["why"], () => [{ name: "x", description: "" }])).toBeNull();
		const names = (sub: string) => (sub === "eval" ? [{ name: "ux-review", description: "skill · draft" }] : []);
		expect(commandCompletions("ev", ["eval", "why"], names)?.map(c => c.value)).toEqual(["eval "]);
		expect(commandCompletions("eval ", ["eval"], names)?.map(c => c.value)).toEqual(["eval init ", "eval trigger ", "eval run "]);
		expect(commandCompletions("eval tr", ["eval"], names)?.map(c => c.value)).toEqual(["eval trigger "]);
		expect(commandCompletions("eval run ux", ["eval"], names)?.map(c => c.value)).toEqual(["eval run ux-review"]);
		expect(commandCompletions("eval nope ux", ["eval"], names)).toBeNull();
	});
});
