// rematch's look in a host TUI, shared by omp (extension/index.ts) and Pi (adapters/pi/index.ts).
// Every surface carries the same mark (`◆ rematch`) and the grammar mark · subject · state, in the
// host theme's semantic colors. Duck-typed on purpose: a theme is anything with `fg(token, text)`
// (`bold`, `italic`, `boxRound` when present), a component is `{ render(width), invalidate() }`;
// no host TUI package is imported. Widths come from `Bun.stringWidth` (ANSI ignored), with a
// code-point fallback where Pi runs under Node. Every renderer returns lines no wider than asked.
// The guards are index.ts's (function declarations: safe across the index.ts ⇄ ui.ts import cycle).
import { type AutoApplied, type Envelope, isRecord, recordsOf, type ReviewAction, textOf } from "./index.ts";

type Json = Record<string, unknown>;

const str = (v: unknown): string => (typeof v === "string" ? v : "");
const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);

// ---------------------------------------------------------------------------------------------
// Theme, width, text

export type UiTheme = {
	fg(token: string, text: string): string;
	bold?(text: string): string;
	italic?(text: string): string;
	boxRound?: { topLeft: string; topRight: string; bottomLeft: string; bottomRight: string; horizontal: string; vertical: string };
};

/** Text as is: RPC, print mode, notifications and the message `content` hosts store. */
export const PLAIN_THEME: UiTheme = { fg: (_token, text) => text };

export type UiComponent = { render(width: number): string[]; invalidate(): void; handleInput?(data: string): void };

const bold = (t: UiTheme, s: string) => (t.bold ? t.bold(s) : s);

/** `◆ rematch`: the mark on every surface (glyph in `accent`, word bold). */
export function mark(t: UiTheme): string {
	return `${t.fg("accent", "◆")} ${bold(t, "rematch")}`;
}

const ANSI = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/y;
const ANSI_ALL = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const graphemes = new Intl.Segmenter(undefined, { granularity: "grapheme" });

export function stripAnsi(s: string): string {
	return s.replace(ANSI_ALL, "");
}

function graphemeWidth(g: string): number {
	const cp = g.codePointAt(0) ?? 0;
	if (cp < 0x20 || (cp >= 0x7f && cp < 0xa0) || /^[\p{Mn}\p{Me}\p{Cf}]+$/u.test(g)) return 0;
	if (/\p{Extended_Pictographic}/u.test(g) && (g.includes("\ufe0f") || cp >= 0x1f000)) return 2;
	const wide =
		(cp >= 0x1100 && cp <= 0x115f) || (cp >= 0x2e80 && cp <= 0xa4cf) || (cp >= 0xac00 && cp <= 0xd7a3) || (cp >= 0xf900 && cp <= 0xfaff) ||
		(cp >= 0xfe30 && cp <= 0xfe4f) || (cp >= 0xff00 && cp <= 0xff60) || (cp >= 0xffe0 && cp <= 0xffe6) || (cp >= 0x20000 && cp <= 0x3fffd);
	return wide ? 2 : 1;
}

/** Terminal columns of `s`; ANSI escapes count zero. */
export function visibleWidth(s: string): number {
	const bun = (globalThis as { Bun?: { stringWidth?(s: string): number } }).Bun;
	if (bun?.stringWidth) return bun.stringWidth(s);
	let w = 0;
	for (const { segment } of graphemes.segment(stripAnsi(s))) w += graphemeWidth(segment);
	return w;
}

/** ANSI escapes and graphemes of `s`, in order. */
function* pieces(s: string): Generator<{ ansi: boolean; text: string }> {
	let i = 0;
	while (i < s.length) {
		ANSI.lastIndex = i;
		const m = ANSI.exec(s);
		if (m) {
			yield { ansi: true, text: m[0] };
			i += m[0].length;
			continue;
		}
		let j = s.indexOf("\x1b", i + 1);
		if (j < 0) j = s.length;
		for (const { segment } of graphemes.segment(s.slice(i, j))) yield { ansi: false, text: segment };
		i = j;
	}
}

/** `s` cut to `width` columns with `…`; styles are reset after a cut. */
export function truncate(s: string, width: number, ellipsis = "…"): string {
	if (width <= 0) return "";
	if (visibleWidth(s) <= width) return s;
	const room = width - visibleWidth(ellipsis);
	let out = "";
	let used = 0;
	let styled = false;
	for (const p of pieces(s)) {
		if (p.ansi) {
			out += p.text;
			styled = true;
			continue;
		}
		const w = visibleWidth(p.text);
		if (used + w > room) break;
		out += p.text;
		used += w;
	}
	return room >= 0 ? `${out}${ellipsis}${styled ? "\x1b[0m" : ""}` : out;
}

/** `s` cut or padded with spaces to exactly `width` columns. */
export function pad(s: string, width: number): string {
	const cut = truncate(s, width);
	return cut + " ".repeat(Math.max(0, width - visibleWidth(cut)));
}

/** Plain text wrapped at word boundaries to `width` columns (long words broken). */
export function wrap(text: string, width: number): string[] {
	const out: string[] = [];
	const w = Math.max(1, width);
	for (const raw of text.replace(/\t/g, "  ").split("\n")) {
		const indent = /^\s*/.exec(raw)?.[0] ?? "";
		let line = "";
		for (const word of raw.slice(indent.length).split(/ +/)) {
			const candidate = line ? `${line} ${word}` : `${indent}${word}`;
			if (visibleWidth(candidate) <= w) {
				line = candidate;
				continue;
			}
			if (line) out.push(line);
			let rest = `${line ? indent : ""}${word}`;
			if (!line) rest = candidate;
			while (visibleWidth(rest) > w) {
				let head = "";
				for (const { segment } of graphemes.segment(rest)) {
					if (visibleWidth(head + segment) > w) break;
					head += segment;
				}
				out.push(head);
				rest = rest.slice(head.length);
			}
			line = rest;
		}
		out.push(line);
	}
	return out;
}

const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
const SHA = /\b[0-9a-f]{40,64}\b/gi;

/** Ids and hashes shortened to 8 characters (`01a180e9…`): the collapsed views never show them whole. */
export function elideIds(s: string): string {
	return s.replace(UUID, m => `${m.slice(0, 8)}…`).replace(SHA, m => `${m.slice(0, 8)}…`);
}

/** A component that renders `build(width)`, the same array while the width is unchanged. */
export function staticComponent(build: (width: number) => string[]): UiComponent {
	let cache: { width: number; lines: string[] } | null = null;
	return {
		render(width: number) {
			if (cache?.width !== width) cache = { width, lines: build(Math.max(1, width)).map(l => truncate(l, Math.max(1, width))) };
			return cache.lines;
		},
		invalidate() {
			cache = null;
		},
	};
}

/** Pick the theme out of a renderer's arguments: omp passes `(args, options, theme)`, Pi `(args, theme, context)`. */
export function themeOf(...candidates: unknown[]): UiTheme {
	for (const c of candidates) if (isRecord(c) && typeof c.fg === "function") return c as unknown as UiTheme;
	return PLAIN_THEME;
}

// ---------------------------------------------------------------------------------------------
// Box

/**
 * A rounded box exactly `width` columns wide: `╭─ title ─── right ─╮`, body lines padded inside
 * `│ … │`, border glyphs in `border` (`borderMuted` disappears on dark themes such as titanium).
 */
export function box(t: UiTheme, width: number, opts: { title: string; right?: string; body: readonly string[] }): string[] {
	if (width < 8) return [opts.title, ...opts.body].map(l => truncate(l, width));
	const g = t.boxRound ?? { topLeft: "╭", topRight: "╮", bottomLeft: "╰", bottomRight: "╯", horizontal: "─", vertical: "│" };
	const b = (s: string) => t.fg("border", s);
	const inner = width - 2;
	// Between the corners: `─ title ` + fill + ` right ` + `─`.
	let right = opts.right ? ` ${opts.right} ` : "";
	let title = opts.title;
	if (inner - visibleWidth(title) - visibleWidth(right) - 4 < 1) right = "";
	if (inner - visibleWidth(title) - 4 < 1) title = truncate(title, Math.max(1, inner - 5));
	const fill = Math.max(0, inner - visibleWidth(title) - visibleWidth(right) - 4);
	const top = `${b(g.topLeft + g.horizontal)} ${title} ${b(g.horizontal.repeat(fill))}${right}${b(g.horizontal + g.topRight)}`;
	const body = opts.body.map(l => `${b(g.vertical)} ${pad(l, inner - 2)} ${b(g.vertical)}`);
	return [truncate(top, width), ...body, b(g.bottomLeft + g.horizontal.repeat(inner) + g.bottomRight)];
}

/** Body lines past `max` collapse to `… N more (ctrl+o)`. */
function collapse(t: UiTheme, lines: string[], max: number, expanded: boolean): string[] {
	if (expanded || lines.length <= max) return lines;
	return [...lines.slice(0, max), t.fg("dim", `… ${lines.length - max} more (ctrl+o)`)];
}

// ---------------------------------------------------------------------------------------------
// 1. Footer status

export type CheckState =
	| { state: "running"; kind: string | null }
	| { state: "done"; kind: string | null; ok: boolean; errors: number; durationMs: number; timedOut: boolean; impact: number };

function seconds(ms: number): string {
	return `${(ms / 1000).toFixed(1)}s`;
}

/** The check segment of the footer: `⠋ cargo` / `✓ cargo 0.4s` / `✗ cargo 2 errors`. */
export function checkSegment(t: UiTheme, c: CheckState): string {
	const kind = c.kind ?? "check";
	if (c.state === "running") return t.fg("dim", `⠋ ${kind}`);
	if (c.timedOut) return t.fg("warning", `◷ ${kind} timed out`);
	if (!c.ok || c.errors > 0) return t.fg("error", `✗ ${kind} ${c.errors} error${c.errors === 1 ? "" : "s"}`);
	const callers = c.impact > 0 ? t.fg("dim", ` · ${c.impact} caller${c.impact === 1 ? "" : "s"}`) : "";
	return `${t.fg("success", `✓ ${kind} ${seconds(c.durationMs)}`)}${callers}`;
}

/**
 * `◆ rematch  ✓ cargo 0.4s · 6 on trial · ● 1 needs you alt+e`, or undefined with nothing to say.
 * `needsYou` counts approvable actions only; blocked ones are not waiting on the user.
 */
export function footerStatus(t: UiTheme, s: { check: CheckState | null; trials: number; needsYou: number; reviewKey: string | null }): string | undefined {
	const parts: string[] = [];
	if (s.check) parts.push(checkSegment(t, s.check));
	if (s.trials > 0) parts.push(t.fg("muted", `${s.trials} on trial`));
	if (s.needsYou > 0) parts.push(`${t.fg("warning", `● ${s.needsYou} needs you`)}${s.reviewKey ? ` ${t.fg("dim", s.reviewKey)}` : ""}`);
	if (parts.length === 0) return undefined;
	return `${mark(t)} ${parts.join(t.fg("dim", " · "))}`;
}

// ---------------------------------------------------------------------------------------------
// 2. Fast-check card

export type CheckDiagnostic = { loc: string; code: string | null; message: string; help: string[] };
export type CheckImpact = { loc: string; symbol: string; text: string };
export type ParsedCheck = { diagnostics: CheckDiagnostic[]; impact: CheckImpact[]; notes: string[] };

/** The binary's rendered check (`rematch-core` `render_report`), the text the agent received, as parts. */
export function parseCheck(rendered: string): ParsedCheck {
	const out: ParsedCheck = { diagnostics: [], impact: [], notes: [] };
	let inImpact = false;
	for (const line of rendered.split("\n")) {
		if (!line.trim() || line.startsWith("[rematch fast-check]")) continue;
		if (line === "impact (references in unedited files):") {
			inImpact = true;
			continue;
		}
		const help = /^\s+help: (.*)$/.exec(line);
		if (help && out.diagnostics.length > 0 && !inImpact) {
			out.diagnostics[out.diagnostics.length - 1]?.help.push(help[1] as string);
			continue;
		}
		const impact = inImpact ? /^\s+(.+?:\d+) \((.+?)\): (.*)$/.exec(line) : null;
		if (impact) {
			out.impact.push({ loc: impact[1] as string, symbol: impact[2] as string, text: (impact[3] as string).trim() });
			continue;
		}
		const diag = /^(\S.*?:\d+:\d+)(?: \[([^\]]+)\])? (.*)$/.exec(line);
		if (diag && !inImpact) {
			out.diagnostics.push({ loc: diag[1] as string, code: diag[2] ?? null, message: diag[3] as string, help: [] });
			continue;
		}
		out.notes.push(line.trim());
	}
	return out;
}

/** `details` of a check message: what the binary reported besides the rendered text. */
export type CheckDetails = { kind?: unknown; ok?: unknown; errors?: unknown; duration_ms?: unknown; timed_out?: unknown };

/** The check state a check message's details stand for (`impact` from its rendered text). */
export function checkStateOf(details: CheckDetails, rendered: string): CheckState & { state: "done" } {
	return {
		state: "done",
		kind: typeof details.kind === "string" ? details.kind : null,
		ok: details.ok === true,
		errors: num(details.errors),
		durationMs: num(details.duration_ms),
		timedOut: details.timed_out === true || /timed out after/.test(rendered.split("\n")[0] ?? ""),
		impact: parseCheck(rendered).impact.length,
	};
}

/**
 * The transcript card of one check run. Clean: one line `◆ rematch · ✓ cargo clean in 0.4s`.
 * Otherwise a box: what the agent was sent, the first 3 diagnostics and the unedited callers;
 * expanded, the exact text the agent received.
 */
export function checkCardLines(t: UiTheme, width: number, rendered: string, details: CheckDetails, expanded: boolean): string[] {
	const c = checkStateOf(details, rendered);
	const kind = c.kind ?? "check";
	const parsed = parseCheck(rendered);
	const clean = !c.timedOut && c.ok && c.errors === 0 && parsed.impact.length === 0 && parsed.diagnostics.length === 0;
	if (clean) return [truncate(`${mark(t)} ${t.fg("dim", "·")} ${t.fg("success", "✓")} ${t.fg("dim", `${kind} clean in ${seconds(c.durationMs)}`)}`, width)];
	const inner = Math.max(1, width - 4);
	const sent = t.fg("dim", "→ sent to the agent");
	let head: string;
	if (c.timedOut) head = `${t.fg("warning", `◷ timed out after ${seconds(c.durationMs)}`)} ${sent}`;
	else if (c.errors > 0 || !c.ok) {
		const n = c.errors || parsed.diagnostics.length;
		head = `${t.fg("error", `✗ ${n} error${n === 1 ? "" : "s"}`)} ${sent}`;
	} else {
		// It compiles: the callers are a hint to look at, not a failure.
		const n = parsed.impact.length;
		head = `${t.fg("success", `✓ ${kind} clean`)} ${t.fg("dim", `· ${n} caller${n === 1 ? "" : "s"} in unedited files`)} ${sent}`;
	}
	const body: string[] = [head];
	if (expanded) {
		for (const line of rendered.split("\n")) for (const w of wrap(line, inner)) body.push(w);
	} else {
		for (const d of parsed.diagnostics.slice(0, 3)) {
			body.push(`  ${t.fg("accent", d.loc)}  ${d.code ? `${t.fg("warning", d.code)}  ` : ""}${d.message}`);
			for (const h of d.help.slice(0, 1)) body.push(t.fg("dim", `  help: ${h}`));
		}
		if (parsed.diagnostics.length > 3) body.push(t.fg("dim", `  … ${parsed.diagnostics.length - 3} more (ctrl+o)`));
		if (parsed.impact.length > 0) {
			const first = parsed.impact[0] as CheckImpact;
			const n = parsed.impact.length;
			body.push(
				`  ${t.fg("warning", "↳")} ${n} caller${n === 1 ? "" : "s"} in unedited files: ${t.fg("accent", first.loc)} ${first.symbol}${n > 1 ? t.fg("dim", " …") : ""}`,
			);
		}
		for (const note of parsed.notes.filter(n => n.startsWith("…["))) body.push(t.fg("dim", `  ${note}`));
	}
	return box(t, width, { title: `${mark(t)} ${t.fg("dim", "·")} fast-check`, right: t.fg("muted", `${kind} · ${seconds(c.durationMs)}`), body });
}

/** `◆ rematch · checking cargo…`: the working message while a check runs. */
export function checkingMessage(t: UiTheme, kind: string | null): string {
	return `${mark(t)} ${t.fg("dim", `· checking ${kind ?? ""}`.trimEnd())}${t.fg("dim", "…")}`;
}

// ---------------------------------------------------------------------------------------------
// Review data in words (names, never ids or hashes)

const KIND_LABEL: Record<string, string> = {
	scripts: "script",
	instructions: "instructions",
	instructions_conflict: "edited instructions",
	instructions_applied: "instructions",
	instructions_unreadable: "instructions",
	instructions_sync: "instructions",
	export: "export",
	ship: "team ship",
	fork: "fork",
	probe: "probe",
};

/** `script`, `hook`, `script+hook`, `edited instructions`, … */
export function kindLabel(kind: string): string {
	return kind
		.split("+")
		.map(k => KIND_LABEL[k] ?? k)
		.join("+");
}

/** Keys and verbs of an action's review, per kind (the binary's `library approve|decline` dispatch). */
export function reviewLabels(kind: string): { approve: string; decline: string } {
	switch (kind) {
		case "instructions_conflict":
			return { approve: "restore", decline: "keep mine" };
		case "instructions_applied":
		case "instructions_unreadable":
			return { approve: "approve", decline: "withdraw" };
		case "export":
		case "ship":
		case "fork":
		case "probe":
			return { approve: "approve", decline: "skip" };
		default:
			return { approve: "approve", decline: "decline" };
	}
}

/** The action's own text with its id read as its name and other ids and hashes shortened. */
function humanText(a: { id: string; name: string }, text: string): string {
	return elideIds(text.split(a.id).join(a.name));
}

function capitalize(s: string): string {
	return s ? s[0]?.toUpperCase() + s.slice(1) : s;
}

export type DiffLine = { type: "file" | "add" | "del" | "ctx"; text: string };

/**
 * An action's diff as display lines: `+++ path (sha256 …)` headers become file rows (the hash
 * dropped), a new file's content is added lines, a `---`/`+++` pair starts a line diff, the
 * sandbox section is left out (shown as Observed), anything else is context.
 */
export function diffLines(diff: string): DiffLine[] {
	const out: DiffLine[] = [];
	const lines = diff.replace(/\n$/, "").split("\n");
	let mode: "raw" | "new" | "unified" | "skip" = "raw";
	const pathOf = (header: string) => header.slice(4).replace(/ \((?:sha256|rematch block|fragment)[^)]*\)\s*$/, "").trim();
	for (let i = 0; i < lines.length; i++) {
		const line = lines[i] as string;
		if (line === "--- sandbox dry run") {
			mode = "skip";
			continue;
		}
		if (line.startsWith("--- ") && lines[i + 1]?.startsWith("+++ ")) {
			out.push({ type: "file", text: pathOf(lines[i + 1] as string) });
			mode = "unified";
			i++;
			continue;
		}
		if (line.startsWith("+++ ") || (line.startsWith("--- ") && mode !== "unified")) {
			out.push({ type: "file", text: pathOf(line) });
			mode = line.startsWith("+++ ") ? "new" : "unified";
			continue;
		}
		if (mode === "skip") continue;
		if (mode === "new") out.push({ type: "add", text: line });
		else if (mode === "unified" && /^[+\- ]/.test(line)) out.push({ type: line[0] === "+" ? "add" : line[0] === "-" ? "del" : "ctx", text: line.slice(1) });
		else out.push({ type: "ctx", text: line });
	}
	while (out.length > 0 && out[out.length - 1]?.type !== "file" && !out[out.length - 1]?.text.trim()) out.pop();
	return out.map(l => ({ ...l, text: elideIds(l.text) }));
}

/** `writes ~/.ledger · no network · exit 1` from an action's `observed`; null without one. */
export function observedText(observed: unknown): string | null {
	if (!isRecord(observed)) return null;
	if (observed.runner === "unsupported") return "no sandbox dry run on this platform";
	const writes = Array.isArray(observed.writes_outside) ? observed.writes_outside.filter((w): w is string => typeof w === "string") : [];
	const parts = [writes.length > 0 ? `writes ${writes.join(", ")}` : "no writes outside", observed.network === true ? "network" : "no network"];
	parts.push(typeof observed.exit_code === "number" ? `exit ${observed.exit_code}` : "killed");
	return parts.join(" · ");
}

/** What approving does, in words: `Make scripts/snapshot.sh executable`, else the action's summary. */
export function actionTitle(a: ReviewAction): string {
	if (a.kind.split("+").includes("scripts")) {
		const files = diffLines(a.diff)
			.filter(l => l.type === "file" && l.text.startsWith("scripts/"))
			.map(l => l.text);
		if (files.length > 0) return `Make ${files.join(", ")} executable`;
	}
	const summary = a.summary.replace(new RegExp(` of [a-z]+ ${a.name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`), "");
	return capitalize(humanText(a, summary || a.name));
}

/** Why an action waits for the user rather than being applied automatically. */
export function whyItWaits(a: ReviewAction): string {
	if (a.blocked) return `blocked: ${humanText(a, a.blocked)}`;
	const parts = a.kind.split("+");
	if (parts.includes("scripts")) {
		const o = isRecord(a.observed) ? a.observed : null;
		const writes = o && Array.isArray(o.writes_outside) && o.writes_outside.length > 0;
		const dirty = o && o.clean !== true && o.runner !== "unsupported";
		const how = writes ? "it wrote outside its sandbox" : o?.network === true ? "it tried the network" : dirty ? "its dry run did not exit cleanly" : null;
		return `scripts can run anything${how ? `; ${how}` : ""}`;
	}
	if (parts.includes("hook")) return "a hook runs inside every omp session";
	if (parts.includes("agent")) return "a subagent is offered in every session";
	switch (a.kind) {
		case "instructions":
			return "personal text loads in every session";
		case "instructions_conflict":
			return "you edited rematch's text in your AGENTS.md";
		case "instructions_applied":
			return "applied; withdraw it to remove it";
		case "export":
			return "copies the entry beyond this machine";
		case "ship":
		case "fork":
			return "proposes the entry to a repo";
		case "probe":
			return "a replay spends model calls";
		default:
			return humanText(a, a.reason.split("\n")[0] ?? "");
	}
}

const MODE_WHAT: Record<string, string> = {
	check: "runs a fast check after edits",
	truncate: "shortens old tool output",
	stuck: "stops repeated identical calls",
	retrieve: "adds matching skill units",
	block: "blocks matching calls",
};

/** What an automatic change does, in words. */
export function appliedWhat(a: AutoApplied): string {
	const hook = /^hook \S+ \((\w+)\)$/.exec(a.what);
	if (hook && MODE_WHAT[hook[1] as string]) return MODE_WHAT[hook[1] as string] as string;
	return elideIds(a.what);
}

/** Why it needed no decision: `data-only`, `read-only`, `rule <rule>`, `probe supported`, `probe regressed`. */
export function appliedWhy(a: { why?: unknown; rule?: unknown }): string {
	switch (a.why) {
		case "rule":
			return typeof a.rule === "string" && a.rule ? `rule ${a.rule}` : "rule";
		case "comparative_support":
			return "probe supported";
		case "probe_regressed":
			return "probe regressed";
		default:
			return str(a.why);
	}
}

// ---------------------------------------------------------------------------------------------
// 3. Session-start digest card and the needs-you widget

export type DigestRow = { glyph: string; tone: string; label: string; name: string; detail: string; hint?: string };

const PROBE_VERDICT: Record<string, string> = { supports: "probe supported it", regresses: "probe found a regression", inconclusive: "probe inconclusive" };

/**
 * The card rows of a digest: what needs you, then what changed since the last digest (kept,
 * retired, newly applied automatically, probe verdicts) and what is on trial here (`trials`:
 * the names the footer counts). `news` is false when nothing changed: then no card is posted.
 */
export function digestRows(data: unknown, needs: readonly ReviewAction[], reviewKey: string | null, trials: readonly string[]): { rows: DigestRow[]; news: boolean } {
	const d = isRecord(data) ? data : {};
	const since = num(d.since);
	const changed = recordsOf(d.changed);
	const auto = new Set(recordsOf(d.applied_automatically).map(a => str(a.id)));
	const rows: DigestRow[] = [];
	for (const a of needs.filter(x => x.needsYou)) {
		rows.push({ glyph: "●", tone: "warning", label: "needs you", name: a.name, detail: observedSummary(a), hint: reviewKey ?? "/rematch review" });
	}
	for (const e of changed) {
		const c = isRecord(e.counters) ? e.counters : {};
		if (e.status === "kept" || e.status === "pinned") {
			const used = num(c.used_clean) || num(c.used);
			rows.push({ glyph: "✓", tone: "success", label: e.status === "pinned" ? "pinned" : "kept", name: str(e.name), detail: used > 0 ? `${used} clean use${used === 1 ? "" : "s"}` : str(e.kind) });
		} else if (e.status === "retired") {
			rows.push({ glyph: "✗", tone: "error", label: "retired", name: str(e.name), detail: elideIds(str(e.note) || `${num(c.harm)} harm · ${num(c.recurrence)} recurrence`) });
		}
	}
	const newAuto = changed.filter(e => e.status !== "retired" && auto.has(str(e.id)));
	if (newAuto.length > 0) {
		const names = newAuto.map(e => str(e.name));
		rows.push({ glyph: "↻", tone: "accent", label: "applied", name: `${names.length} automatically`, detail: listOf(names), hint: reviewKey ? `undo: ${reviewKey}` : undefined });
	}
	for (const p of recordsOf(d.probes).filter(p => num(p.at) > since && PROBE_VERDICT[str(p.status)])) {
		rows.push({ glyph: "◆", tone: str(p.status) === "regresses" ? "error" : "accent", label: "probe", name: str(p.name), detail: PROBE_VERDICT[str(p.status)] as string });
	}
	const news = rows.some(r => r.label !== "needs you");
	if (trials.length > 0) rows.push({ glyph: "◇", tone: "muted", label: "on trial", name: `${trials.length} here`, detail: listOf(trials) });
	return { rows, news };
}

function listOf(names: readonly string[], max = 3): string {
	return `(${names.slice(0, max).join(", ")}${names.length > max ? ", …" : ""})`;
}

/** `script writes ~/.ledger` for a needs-you row: kind plus what it did, else its title. */
export function observedSummary(a: ReviewAction): string {
	const o = isRecord(a.observed) ? a.observed : null;
	const writes = o && Array.isArray(o.writes_outside) ? o.writes_outside.filter((w): w is string => typeof w === "string") : [];
	if (writes.length > 0) return `${kindLabel(a.kind)} writes ${writes.join(", ")}`;
	if (o?.network === true) return `${kindLabel(a.kind)} uses the network`;
	return `${kindLabel(a.kind)} · ${actionTitle(a)}`;
}

function rowLine(t: UiTheme, r: DigestRow, nameWidth: number, width: number): string {
	const left = `${t.fg(r.tone, `${r.glyph} ${r.label.padEnd(9)}`)}  ${bold(t, pad(r.name, nameWidth))}  `;
	const hint = r.hint ? `  ${t.fg("dim", r.hint)}` : "";
	const room = width - visibleWidth(left) - visibleWidth(hint);
	if (room < 4) return truncate(`${left}${t.fg("muted", r.detail)}`, width);
	return `${left}${pad(t.fg("muted", r.detail), room)}${hint}`;
}

/** The digest card: `╭─ ◆ rematch ─…` with one row per change (at most 8, then `… N more`). */
export function digestCardLines(t: UiTheme, width: number, rows: readonly DigestRow[], expanded: boolean): string[] {
	const inner = Math.max(1, width - 4);
	const nameWidth = Math.min(24, Math.max(4, ...rows.map(r => visibleWidth(r.name))));
	const body = rows.map(r => rowLine(t, r, nameWidth, inner));
	return box(t, width, { title: mark(t), right: t.fg("dim", "since last session"), body: collapse(t, body, 8, expanded) });
}

/** The digest rows as plain text: the stored message content and the RPC/print rendering. */
export function digestText(rows: readonly DigestRow[]): string {
	return ["rematch", ...rows.map(r => `${r.glyph} ${r.label.padEnd(9)}  ${r.name}  ${r.detail}${r.hint ? `  ${r.hint}` : ""}`)].join("\n");
}

/** `◆ rematch  ● 1 needs you: ledger-snapshot (script writes ~/.ledger)   alt+e review`. */
export function needsYouLine(t: UiTheme, needs: readonly ReviewAction[], reviewKey: string | null): string {
	const first = needs[0];
	if (!first) return "";
	const more = needs.length > 1 ? t.fg("dim", ` +${needs.length - 1} more`) : "";
	return `${mark(t)}  ${t.fg("warning", `● ${needs.length} needs you:`)} ${bold(t, first.name)} ${t.fg("muted", `(${observedSummary(first)})`)}${more}   ${t.fg("dim", `${reviewKey ?? "/rematch"} review`)}`;
}

// ---------------------------------------------------------------------------------------------
// 4. Review panel: a pure state machine plus its rendering

/** A key from raw terminal input: legacy sequences and the Kitty keyboard protocol. */
export function keyOf(data: string): string {
	const names: Record<string, string> = {
		"\x1b": "escape", "\r": "enter", "\n": "enter", "\t": "tab", "\x1b[Z": "shift-tab", "\x7f": "backspace",
		"\x1b[A": "up", "\x1b[B": "down", "\x1b[C": "right", "\x1b[D": "left", "\x1bOA": "up", "\x1bOB": "down", "\x1bOC": "right", "\x1bOD": "left",
	};
	const named = names[data];
	if (named) return named;
	const arrow = /^\x1b\[1;\d+(?::\d+)?([ABCD])$/.exec(data);
	if (arrow) return ({ A: "up", B: "down", C: "right", D: "left" } as Record<string, string>)[arrow[1] as string] as string;
	const csiU = /^\x1b\[(\d+)(?::\d+)*(?:;(\d+)(?::\d+)?)?u$/.exec(data);
	if (csiU) {
		const code = Number(csiU[1]);
		const shift = ((Number(csiU[2] ?? 1) - 1) & 1) === 1;
		if (code === 27) return "escape";
		if (code === 13) return "enter";
		if (code === 9) return shift ? "shift-tab" : "tab";
		if (code === 127) return "backspace";
		return String.fromCodePoint(code);
	}
	return data;
}

export type PanelTab = "needs" | "applied";
export type PanelNotice = { tone: "success" | "error" | "muted"; text: string };
export type ReviewPanelState = {
	tab: PanelTab;
	needs: ReviewAction[];
	applied: AutoApplied[];
	cursor: Record<PanelTab, number>;
	fullDiff: boolean;
	/** The id of the automatic change whose undo waits for y/n. */
	confirmUndo: string | null;
	/** What runs now; keys wait meanwhile. */
	busy: string | null;
	notice: PanelNotice | null;
};
export type PanelEffect =
	| { type: "approve" | "decline"; action: ReviewAction; argv: string[] }
	| { type: "undo"; item: AutoApplied; argv: string[] }
	| { type: "close" };

export function reviewPanel(needs: readonly ReviewAction[], applied: readonly AutoApplied[]): ReviewPanelState {
	return {
		tab: needs.length > 0 || applied.length === 0 ? "needs" : "applied",
		needs: [...needs],
		applied: [...applied],
		cursor: { needs: 0, applied: 0 },
		fullDiff: false,
		confirmUndo: null,
		busy: null,
		notice: null,
	};
}

const clampCursor = (s: ReviewPanelState): ReviewPanelState => ({
	...s,
	cursor: { needs: Math.max(0, Math.min(s.cursor.needs, s.needs.length - 1)), applied: Math.max(0, Math.min(s.cursor.applied, s.applied.length - 1)) },
});

/** The item under the cursor of the current tab. */
export function panelCurrent(s: ReviewPanelState): ReviewAction | AutoApplied | undefined {
	return s.tab === "needs" ? s.needs[s.cursor.needs] : s.applied[s.cursor.applied];
}

/**
 * One key: the next state and what to run. `a`/`d` approve or decline the action under the
 * cursor (its own bound argv), `u` asks to undo an automatic change (`y` runs it, `n`/esc
 * cancels), `i` toggles the full diff, ↑↓/j/k move, tab switches tabs, esc/q close. While
 * something runs every key waits.
 */
export function panelKey(s: ReviewPanelState, key: string): { state: ReviewPanelState; effect: PanelEffect | null } {
	const same = { state: s, effect: null };
	if (s.busy) return same;
	if (s.confirmUndo) {
		const item = s.applied.find(x => x.id === s.confirmUndo);
		if (key === "y" && item?.undo_argv) {
			return { state: { ...s, confirmUndo: null, busy: `undoing ${item.name}…`, notice: null }, effect: { type: "undo", item, argv: item.undo_argv } };
		}
		if (key === "n" || key === "escape") return { state: { ...s, confirmUndo: null }, effect: null };
		return same;
	}
	switch (key) {
		case "escape":
		case "q":
			return { state: s, effect: { type: "close" } };
		case "tab":
		case "shift-tab":
		case "left":
		case "right":
			return { state: { ...s, tab: s.tab === "needs" ? "applied" : "needs", fullDiff: false, notice: null }, effect: null };
		case "up":
		case "k":
			return { state: clampCursor({ ...s, cursor: { ...s.cursor, [s.tab]: s.cursor[s.tab] - 1 }, fullDiff: false, notice: null }), effect: null };
		case "down":
		case "j":
			return { state: clampCursor({ ...s, cursor: { ...s.cursor, [s.tab]: s.cursor[s.tab] + 1 }, fullDiff: false, notice: null }), effect: null };
	}
	if (s.tab === "needs") {
		const a = s.needs[s.cursor.needs];
		if (!a) return same;
		const labels = reviewLabels(a.kind);
		if (key === "i") return { state: { ...s, fullDiff: !s.fullDiff }, effect: null };
		if (key === "a") {
			if (!a.approve || a.blocked) return { state: { ...s, notice: { tone: "muted", text: a.blocked ? "it cannot be approved as is (see why it waits)" : `nothing to ${labels.approve}` } }, effect: null };
			return { state: { ...s, busy: `${labels.approve === "approve" ? "approving" : "restoring"} ${a.name}…`, notice: null }, effect: { type: "approve", action: a, argv: a.approve } };
		}
		if (key === "d") {
			if (!a.decline) return { state: { ...s, notice: { tone: "muted", text: `nothing to ${labels.decline}` } }, effect: null };
			return { state: { ...s, busy: `${labels.decline}: ${a.name}…`, notice: null }, effect: { type: "decline", action: a, argv: a.decline } };
		}
		return same;
	}
	const item = s.applied[s.cursor.applied];
	if (key === "u" && item) {
		if (!item.undo_argv) return { state: { ...s, notice: { tone: "muted", text: `${item.name}: nothing undoes it (${appliedWhy(item)})` } }, effect: null };
		return { state: { ...s, confirmUndo: item.id, notice: null }, effect: null };
	}
	return same;
}

/** The state after an effect ran: the item leaves its list on success; the result shows inline. */
export function panelResult(s: ReviewPanelState, effect: PanelEffect, ok: boolean, error?: string): ReviewPanelState {
	if (effect.type === "close") return s;
	const name = effect.type === "undo" ? effect.item.name : effect.action.name;
	if (!ok) return { ...s, busy: null, notice: { tone: "error", text: `✗ ${effect.type} ${name} failed: ${elideIds(error ?? "unknown error")}` } };
	const verb =
		effect.type === "undo" ? "undid" : effect.type === "approve" ? (reviewLabels(effect.action.kind).approve === "restore" ? "restored" : "approved") : reviewLabels(effect.action.kind).decline === "keep mine" ? "kept yours:" : `${reviewLabels(effect.action.kind).decline === "withdraw" ? "withdrew" : reviewLabels(effect.action.kind).decline === "skip" ? "skipped" : "declined"}`;
	const next =
		effect.type === "undo"
			? { ...s, applied: s.applied.filter(x => x.id !== effect.item.id) }
			: { ...s, needs: s.needs.filter(x => x.id !== effect.action.id) };
	return clampCursor({ ...next, busy: null, fullDiff: false, notice: { tone: "success", text: `✓ ${verb} ${name}` } });
}

/** Nothing left in either tab: the panel closes. */
export function panelEmpty(s: ReviewPanelState): boolean {
	return s.needs.length === 0 && s.applied.length === 0;
}

function keyHints(t: UiTheme, hints: readonly [string, string][]): string {
	return hints.map(([k, label]) => `${t.fg("accent", k)} ${t.fg("dim", label)}`).join("  ");
}

function labelRow(t: UiTheme, label: string, value: string, inner: number): string[] {
	const head = t.fg("muted", label.padEnd(14));
	return wrap(value, Math.max(8, inner - 16)).map((l, i) => `  ${i === 0 ? head : " ".repeat(14)}${l}`);
}

function windowAround<T>(items: readonly T[], cursor: number, size: number): { start: number; items: readonly T[] } {
	const start = Math.max(0, Math.min(cursor - Math.floor(size / 2), items.length - size));
	return { start, items: items.slice(start, start + size) };
}

/** The panel at `width`: tabs in the title, the list, the current item's details, the result line and the keys. */
export function reviewPanelLines(t: UiTheme, width: number, s: ReviewPanelState): string[] {
	const inner = Math.max(1, width - 4);
	const tab = (id: PanelTab, label: string, n: number) => (s.tab === id ? bold(t, t.fg("accent", `[${label} ${n}]`)) : t.fg("muted", `${label} ${n}`));
	const right = `${tab("needs", "Needs you", s.needs.length)} ${tab("applied", "Applied", s.applied.length)}`;
	const body: string[] = [];
	const nameWidth = Math.min(28, Math.max(8, ...(s.tab === "needs" ? s.needs : s.applied).map(x => visibleWidth(x.name))));
	const row = (selected: boolean, glyph: string, name: string, meta: string) =>
		`${selected ? t.fg("accent", "❯") : " "} ${glyph} ${selected ? bold(t, pad(name, nameWidth)) : pad(name, nameWidth)}  ${t.fg("muted", meta)}`;
	if (s.tab === "needs") {
		if (s.needs.length === 0) body.push(t.fg("muted", "  Nothing needs you."));
		const { start, items } = windowAround(s.needs, s.cursor.needs, 5);
		items.forEach((a, i) => {
			const glyph = a.blocked ? t.fg("error", "⊘") : a.state === "conflict" ? t.fg("warning", "◆") : t.fg("warning", "●");
			body.push(row(start + i === s.cursor.needs, glyph, a.name, [kindLabel(a.kind), a.place].filter(Boolean).join(" · ")));
		});
		const a = s.needs[s.cursor.needs];
		if (a) {
			body.push("");
			for (const l of wrap(actionTitle(a), inner - 2)) body.push(`  ${bold(t, l)}`);
			body.push(...labelRow(t, "Why it waits", whyItWaits(a), inner));
			const observed = observedText(a.observed);
			if (observed) body.push(...labelRow(t, "Observed", observed, inner));
			const diff = diffLines(a.diff);
			const max = s.fullDiff ? 40 : 6;
			for (const d of diff.slice(0, max)) {
				if (d.type === "file") body.push(`  ${t.fg("muted", `┄ ${d.text} ┄`)}`);
				else if (d.type === "add") body.push(`  ${t.fg("toolDiffAdded", `+ ${d.text}`)}`);
				else if (d.type === "del") body.push(`  ${t.fg("toolDiffRemoved", `- ${d.text}`)}`);
				else body.push(`  ${t.fg("toolDiffContext", `  ${d.text}`)}`);
			}
			if (diff.length > max) body.push(t.fg("dim", `  … ${diff.length - max} more line${diff.length - max === 1 ? "" : "s"}${s.fullDiff ? " (rematch library inspect)" : " (i)"}`));
		}
	} else {
		if (s.applied.length === 0) body.push(t.fg("muted", "  Nothing was applied automatically."));
		const { start, items } = windowAround(s.applied, s.cursor.applied, 6);
		items.forEach((x, i) => body.push(row(start + i === s.cursor.applied, t.fg("accent", "↻"), x.name, `${x.kind} · ${appliedWhy(x)}`)));
		const x = s.applied[s.cursor.applied];
		if (x) {
			body.push("");
			for (const l of wrap(capitalize(appliedWhat(x)), inner - 2)) body.push(`  ${bold(t, l)}`);
			body.push(...labelRow(t, "Why automatic", appliedWhy(x), inner));
			body.push(...labelRow(t, "Undo", x.undo_argv ? `rematch ${x.undo_argv.join(" ")}` : "nothing undoes it", inner));
		}
	}
	body.push("");
	if (s.busy) body.push(t.fg("dim", `  ⠋ ${s.busy}`));
	else if (s.notice) body.push(`  ${t.fg(s.notice.tone, s.notice.text)}`);
	const current = panelCurrent(s);
	if (s.confirmUndo && current && "undo_argv" in current) {
		body.push(`  ${t.fg("warning", `Undo ${current.name}?`)} ${t.fg("dim", `runs rematch ${(current.undo_argv ?? []).join(" ")}`)}   ${keyHints(t, [["y", "yes"], ["n", "no"]])}`);
	} else if (s.tab === "needs") {
		const labels = reviewLabels(s.needs[s.cursor.needs]?.kind ?? "");
		body.push(` ${keyHints(t, [["a", labels.approve], ["d", labels.decline], ["i", s.fullDiff ? "short diff" : "full diff"], ["↑↓", "item"], ["tab", "applied"], ["esc", "later"]])}`);
	} else {
		body.push(` ${keyHints(t, [["u", "undo"], ["↑↓", "item"], ["tab", "needs you"], ["esc", "close"]])}`);
	}
	const title = `${mark(t)} ${t.fg("dim", "·")} review`;
	return box(t, width, { title, right, body });
}

/**
 * The panel as a component: keys go through [`panelKey`], effects run through `run` (the
 * host's binary call), results show inline; it finishes on esc or when nothing is left.
 */
export function reviewPanelComponent(
	t: UiTheme,
	host: { requestRender(): void },
	initial: ReviewPanelState,
	run: (effect: Exclude<PanelEffect, { type: "close" }>) => Promise<{ ok: boolean; error?: string }>,
	done: (changed: boolean) => void,
): UiComponent {
	let state = initial;
	let changed = false;
	let cache: { width: number; state: ReviewPanelState; lines: string[] } | null = null;
	return {
		render(width: number) {
			const w = Math.max(1, Math.min(width, 110));
			if (cache?.width !== w || cache.state !== state) cache = { width: w, state, lines: reviewPanelLines(t, w, state).map(l => truncate(l, w)) };
			return cache.lines;
		},
		invalidate() {
			cache = null;
		},
		handleInput(data: string) {
			const { state: next, effect } = panelKey(state, keyOf(data));
			state = next;
			host.requestRender();
			if (!effect) return;
			if (effect.type === "close") {
				done(changed);
				return;
			}
			void run(effect)
				.catch(error => ({ ok: false, error: error instanceof Error ? error.message : String(error) }))
				.then(r => {
					if (r.ok) changed = true;
					state = panelResult(state, effect, r.ok, r.error);
					host.requestRender();
					if (panelEmpty(state)) done(changed);
				});
		},
	};
}

// ---------------------------------------------------------------------------------------------
// 5. `/rematch …` output and 6. the agent's `rematch` tool

export type LineClass = "needs" | "blocked" | "applied" | "kept" | "retired" | "trial" | "ok" | "error" | "detail" | "plain";

const LINE_STYLE: Record<LineClass, { glyph: string; tone: string | null }> = {
	needs: { glyph: "●", tone: "warning" },
	blocked: { glyph: "⊘", tone: "error" },
	applied: { glyph: "↻", tone: "accent" },
	kept: { glyph: "✓", tone: "success" },
	retired: { glyph: "✗", tone: "muted" },
	trial: { glyph: "◇", tone: "muted" },
	ok: { glyph: "✓", tone: "success" },
	error: { glyph: "✗", tone: "error" },
	detail: { glyph: " ", tone: "dim" },
	plain: { glyph: "·", tone: null },
};

/** The class of one output line of the binary, by its prefix (the generic renderer's colors). */
export function classifyLine(line: string): LineClass {
	const l = line.trimStart().toLowerCase();
	if (/^\s/.test(line) && line.trim()) return "detail";
	if (/^(approval pending|conflict:|waiting|needs you)/.test(l)) return "needs";
	if (/^blocked\b/.test(l)) return "blocked";
	if (/^applied automatically|^applied:/.test(l)) return "applied";
	if (/^(kept|pinned|approved|restored)\b/.test(l)) return "kept";
	if (/^(retired|disabled|declined|withdrew|rolled back)\b/.test(l)) return "retired";
	if (/^(provisional|staged|draft|on trial|trial)\b/.test(l)) return "trial";
	if (/^(✓|ok\b|clean\b|done\b)/.test(l)) return "ok";
	if (/^(✗|error\b|failed\b)|\bfailed:/.test(l)) return "error";
	return "plain";
}

function styledLine(t: UiTheme, line: string, cls: LineClass, expanded: boolean): string {
	const { glyph, tone } = LINE_STYLE[cls];
	const text = expanded ? line : elideIds(line);
	if (cls === "detail") return t.fg("dim", text);
	return `${tone ? t.fg(tone, glyph) : t.fg("dim", glyph)} ${cls === "retired" ? t.fg("muted", text) : text}`;
}

type TableRow = { glyph: string; tone: string; name: string; meta: string; counters: string };

function countersText(c: unknown): string {
	if (!isRecord(c)) return "";
	return `${num(c.used)} used · ${num(c.harm)} harm`;
}

/** `library ls` / `status` / `digest` data as sections (Needs you, Blocked, On trial, Kept, Applied automatically, Retired, Other). */
export function groupedSections(data: unknown): { title: string; rows: TableRow[] }[] | null {
	if (!isRecord(data)) return null;
	const actions = recordsOf(data.actions);
	const entries = recordsOf(data.entries);
	const trials = recordsOf(data.trials);
	const auto = recordsOf(data.applied_automatically);
	if (actions.length === 0 && entries.length === 0 && trials.length === 0 && auto.length === 0) return null;
	const place = (e: Json) => str(e.branch) || str(e.place);
	const sections: { title: string; rows: TableRow[] }[] = [];
	const add = (title: string, rows: TableRow[]) => {
		if (rows.length > 0) sections.push({ title, rows });
	};
	const action = (a: Json, glyph: string, tone: string): TableRow => ({
		glyph,
		tone,
		name: str(a.name),
		meta: [kindLabel(str(a.kind)), str(a.place)].filter(Boolean).join(" · "),
		counters: "",
	});
	add("Needs you", actions.filter(a => !a.blocked && a.state === "pending").map(a => action(a, "●", "warning")));
	add("Blocked", actions.filter(a => a.blocked).map(a => action(a, "⊘", "error")));
	add("Conflicts", actions.filter(a => !a.blocked && a.state === "conflict").map(a => action(a, "◆", "warning")));
	const entryRow = (e: Json, glyph: string, tone: string): TableRow => ({ glyph, tone, name: str(e.name), meta: [str(e.kind), place(e)].filter(Boolean).join(" · "), counters: countersText(e.counters) });
	const byStatus = (...st: string[]) => entries.filter(e => st.includes(str(e.status)));
	const onTrial = entries.length > 0 ? byStatus("provisional") : trials;
	add("On trial", onTrial.map(e => entryRow(e, "◇", "muted")));
	add("Kept", byStatus("kept", "pinned").map(e => entryRow(e, "✓", "success")));
	add("Applied automatically", auto.map(a => ({ glyph: "↻", tone: "accent", name: str(a.name), meta: `${str(a.kind) || "entry"} · ${appliedWhy(a)}`, counters: "" })));
	add("Retired", byStatus("retired").map(e => entryRow(e, "✗", "muted")));
	add("Other", byStatus("draft", "dormant", "stale").map(e => ({ ...entryRow(e, "·", "dim"), meta: `${str(e.status)} · ${str(e.kind)}` })));
	return sections;
}

function sectionLines(t: UiTheme, inner: number, sections: { title: string; rows: TableRow[] }[]): string[] {
	const all = sections.flatMap(s => s.rows);
	const nameW = Math.min(28, Math.max(6, ...all.map(r => visibleWidth(r.name))));
	const metaW = Math.min(26, Math.max(6, ...all.map(r => visibleWidth(r.meta))));
	const out: string[] = [];
	for (const s of sections) {
		out.push(bold(t, `${s.title} ${t.fg("dim", String(s.rows.length))}`));
		for (const r of s.rows) {
			const line = `${t.fg(r.tone, r.glyph)} ${pad(r.name, nameW)}  ${t.fg("muted", pad(r.meta, metaW))}  ${t.fg("dim", r.counters)}`;
			out.push(truncate(line.trimEnd(), inner));
		}
	}
	return out;
}

/** `details` of a `/rematch` output message: the binary's envelope plus the command typed. */
export type OutputDetails = Envelope & { command?: string };

/**
 * A `/rematch …` result: grouped tables for library/status/digest data, else the binary's lines
 * colored by [`classifyLine`]; collapsed to 12 rows, ids shortened unless expanded.
 */
export function outputLines(t: UiTheme, width: number, details: OutputDetails, content: string, expanded: boolean): string[] {
	const inner = Math.max(1, width - 4);
	const title = `${mark(t)} ${t.fg("dim", "·")} ${details.command || details.operation || "output"}`;
	if (!details.ok) {
		const body = wrap(elideIds(details.error ?? "failed"), inner - 2).map((l, i) => (i === 0 ? `${t.fg("error", "✗")} ${l}` : `  ${l}`));
		return box(t, width, { title, right: t.fg("error", "failed"), body });
	}
	const sections = groupedSections(details.data);
	let body: string[];
	if (sections && sections.length > 0) body = sectionLines(t, inner, sections);
	else {
		const lines = content.split("\n").filter((l, i, all) => l.trim() || (i > 0 && i < all.length - 1));
		body = lines.flatMap(l => {
			const cls = classifyLine(l);
			if (!expanded) return [truncate(styledLine(t, l, cls, false), inner)];
			// Expanded: the whole text, wrapped, nothing shortened.
			return wrap(l, inner - 2).map((piece, i) => (i === 0 ? styledLine(t, piece, cls, true) : `  ${t.fg("dim", piece)}`));
		});
	}
	return box(t, width, { title, body: collapse(t, body, 12, expanded) });
}

/** One line for a tool result: `✓ 6 entries`, `✓ 2 waiting · 6 on trial`, `✗ error …`. */
export function summarizeEnvelope(env: Envelope): { ok: boolean; text: string } {
	if (!env.ok) return { ok: false, text: elideIds(env.error ?? "failed") };
	const d = isRecord(env.data) ? env.data : {};
	if (Array.isArray(d.entries)) return { ok: true, text: `${d.entries.length} entr${d.entries.length === 1 ? "y" : "ies"}` };
	if (Array.isArray(d.actions) || Array.isArray(d.trials)) {
		const waiting = recordsOf(d.actions).filter(a => !a.blocked && a.state === "pending").length;
		return { ok: true, text: `${waiting} waiting · ${recordsOf(d.trials).length} on trial` };
	}
	if (typeof d.job_id === "string") return { ok: true, text: `job started (${elideIds(d.job_id)})` };
	const lines = Array.isArray(d.lines) ? d.lines.filter((l): l is string => typeof l === "string") : [];
	if (lines.length === 1) return { ok: true, text: elideIds(lines[0] as string) };
	if (lines.length > 1) return { ok: true, text: `${lines.length} lines` };
	return { ok: true, text: "done" };
}

/** `◆ rematch library ls`: the agent's tool call. */
export function toolCallLine(t: UiTheme, operation: string, args: readonly string[]): string {
	return `${mark(t)} ${bold(t, operation)}${args.length > 0 ? ` ${t.fg("muted", elideIds(args.join(" ")))}` : ""}`;
}

/** `✓ 6 entries` / `✗ …` for a tool result. */
export function toolResultLine(t: UiTheme, env: Envelope): string {
	const s = summarizeEnvelope(env);
	return s.ok ? `${t.fg("success", "✓")} ${t.fg("muted", s.text)}` : `${t.fg("error", "✗")} ${t.fg("error", s.text)}`;
}

// ---------------------------------------------------------------------------------------------
// Host glue shared by omp and Pi: the persistent surfaces and the transcript renderers

/** What the persistent surfaces need of a handler context (omp and Pi alike). */
export type UiContext = {
	mode?: string;
	ui: {
		theme?: unknown;
		setStatus(key: string, text: string | undefined): void;
		setWidget?(key: string, content: unknown, options?: { placement?: "aboveEditor" | "belowEditor" }): void;
	};
};

/** The theme of a handler context: the host's in the TUI, plain text in RPC, print and tests. */
export function contextTheme(ctx: { mode?: string; ui: { theme?: unknown } }): UiTheme {
	return ctx.mode === "tui" ? themeOf(ctx.ui.theme) : PLAIN_THEME;
}

/**
 * The footer status and the needs-you widget, kept in step with what rematch knows: the actions
 * of the last digest, the trials loaded here, the last check. `show` redraws both; the widget
 * is there only while something needs the user.
 */
export class StatusBoard {
	needs: ReviewAction[] = [];
	applied: AutoApplied[] = [];
	/** Names of the entries on trial here. */
	trials: string[] = [];
	check: CheckState | null = null;
	#widget = "";

	/** `reviewKey`: the shortcut that opens the review (`alt+e` in omp), null where there is none. */
	constructor(readonly reviewKey: string | null) {}

	/** Approvable actions only: a blocked one waits on something else. */
	get waiting(): ReviewAction[] {
		return this.needs.filter(a => a.needsYou);
	}

	show(ctx: UiContext): void {
		const t = contextTheme(ctx);
		const waiting = this.waiting;
		// The widget above the editor names what waits; the footer repeats the count only where no widget can show.
		const widget = typeof ctx.ui.setWidget === "function";
		ctx.ui.setStatus("rematch", footerStatus(t, { check: this.check, trials: this.trials.length, needsYou: widget ? 0 : waiting.length, reviewKey: this.reviewKey }));
		if (!widget) return;
		const signature = `${ctx.mode}\0${waiting.map(a => a.id).join("\0")}`;
		if (signature === this.#widget) return;
		this.#widget = signature;
		if (waiting.length === 0) ctx.ui.setWidget("rematch", undefined);
		else if (ctx.mode === "tui") {
			const key = this.reviewKey;
			ctx.ui.setWidget("rematch", (_tui: unknown, theme: unknown) => staticComponent(() => [needsYouLine(themeOf(theme), waiting, key)]), { placement: "aboveEditor" });
		} else ctx.ui.setWidget("rematch", [needsYouLine(PLAIN_THEME, waiting, this.reviewKey)], { placement: "aboveEditor" });
	}
}

type RenderedMessage = { content: unknown; details?: unknown };

/** Renderer of a check card (`registerMessageRenderer`; omp's aside, Pi's view copy). */
export function renderCheckMessage(message: RenderedMessage, options: { expanded: boolean }, theme: unknown): UiComponent {
	const details = isRecord(message.details) ? message.details : {};
	return staticComponent(w => checkCardLines(themeOf(theme), w, textOf(message.content), details, options.expanded));
}

/** Renderer of the session-start digest card. */
export function renderDigestMessage(message: RenderedMessage, options: { expanded: boolean }, theme: unknown): UiComponent {
	const rows = isRecord(message.details) && Array.isArray(message.details.rows) ? (message.details.rows as DigestRow[]) : [];
	return staticComponent(w => digestCardLines(themeOf(theme), w, rows, options.expanded));
}

/** Renderer of `/rematch …` output (`details`: the envelope plus the command typed). */
export function renderOutputMessage(message: RenderedMessage, options: { expanded: boolean }, theme: unknown): UiComponent {
	const details: OutputDetails = isRecord(message.details) && typeof message.details.ok === "boolean" ? (message.details as OutputDetails) : { ok: true };
	return staticComponent(w => outputLines(themeOf(theme), w, details, textOf(message.content), options.expanded));
}

/** `renderCall` of the `rematch` tool, either host's argument order. */
export function renderToolCall(args: unknown, a: unknown, b: unknown): UiComponent {
	const p = isRecord(args) ? args : {};
	const argv = Array.isArray(p.arguments) ? p.arguments.filter((x): x is string => typeof x === "string") : [];
	return staticComponent(() => [toolCallLine(themeOf(b, a), str(p.operation), argv)]);
}

/** `renderResult` of the `rematch` tool: one line, the whole output when expanded. */
export function renderToolResult(result: { details?: unknown }, options: { expanded?: boolean }, theme: unknown, args?: unknown): UiComponent {
	const env: Envelope = isRecord(result.details) && typeof result.details.ok === "boolean" ? (result.details as Envelope) : { ok: false, error: "no result" };
	const t = themeOf(theme, options);
	if (!options.expanded) return staticComponent(() => [toolResultLine(t, env)]);
	const p = isRecord(args) ? args : {};
	const command = [str(p.operation), ...(Array.isArray(p.arguments) ? p.arguments.filter((x): x is string => typeof x === "string") : [])].join(" ");
	const lines = isRecord(env.data) && Array.isArray(env.data.lines) ? env.data.lines.filter((l): l is string => typeof l === "string") : [];
	return staticComponent(w => outputLines(t, w, { ...env, command }, lines.join("\n"), true));
}

// ---------------------------------------------------------------------------------------------
// 7. Autocomplete

export type Completion = { value: string; label: string; description?: string };

const TAKES_NAME = ["approve", "decline", "inspect", "disable", "rollback"];

/**
 * `/rematch <tab>`: the subcommands; `/rematch approve|decline|inspect|disable|rollback <tab>`:
 * the names `namesFor(subcommand)` offers (what waits, what loads here). Values are the whole
 * argument text, which the host puts in place of what was typed.
 */
export function commandCompletions(
	prefix: string,
	commands: readonly string[],
	namesFor: (subcommand: string) => readonly { name: string; description: string }[],
): Completion[] | null {
	const words = prefix.trimStart().split(/\s+/);
	if (words.length <= 1) {
		const w = words[0] ?? "";
		const items = commands.filter(c => c.startsWith(w)).map(c => ({ value: `${c} `, label: c }));
		return items.length > 0 ? items : null;
	}
	const [sub = "", partial = ""] = words;
	if (words.length > 2 || !TAKES_NAME.includes(sub)) return null;
	const seen = new Set<string>();
	const items = namesFor(sub)
		.filter(n => n.name.startsWith(partial) && !seen.has(n.name) && seen.add(n.name))
		.map(n => ({ value: `${sub} ${n.name}`, label: n.name, description: n.description }));
	return items.length > 0 ? items : null;
}
