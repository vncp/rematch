// omp-evolve extension: traces main sessions into <root>/traces/<session_id>.jsonl (DESIGN §3),
// loads matching staging dirs via `skills.customDirectories`, runs approved hooks on `tool_call`,
// and exposes the `omp_evolve` tool, the `/evolve` command and the `alt+e` digest shortcut (DESIGN §10).
// It parses nothing the binary owns: every decision beyond the cheap prelabels lives in Rust.
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import type { ExecResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

// ---------------------------------------------------------------------------------------------
// Paths, kill switch, binary resolution

export function evolveRoot(env: NodeJS.ProcessEnv = process.env): string {
	if (env.OMP_EVOLVE_ROOT) return env.OMP_EVOLVE_ROOT;
	return path.join(env.HOME || os.homedir(), ".omp", "evolve");
}

function isExecutable(p: string): boolean {
	try {
		fs.accessSync(p, fs.constants.X_OK);
		return fs.statSync(p).isFile();
	} catch {
		return false;
	}
}

function extensionDir(): string {
	const dir = path.dirname(fileURLToPath(import.meta.url));
	try {
		return fs.realpathSync(dir);
	} catch {
		return dir;
	}
}

/** `$OMP_EVOLVE_BIN` → `<ext>/../target/release` → `<ext>/../target/debug` → PATH. */
export function resolveBinary(env: NodeJS.ProcessEnv = process.env, extDir = extensionDir()): string | null {
	if (env.OMP_EVOLVE_BIN) return isExecutable(env.OMP_EVOLVE_BIN) ? env.OMP_EVOLVE_BIN : null;
	for (const profile of ["release", "debug"]) {
		const candidate = path.normalize(path.join(extDir, "..", "target", profile, "omp-evolve"));
		if (isExecutable(candidate)) return candidate;
	}
	for (const dir of (env.PATH ?? "").split(path.delimiter)) {
		if (!dir) continue;
		const candidate = path.join(dir, "omp-evolve");
		if (isExecutable(candidate)) return candidate;
	}
	return null;
}

// ---------------------------------------------------------------------------------------------
// Envelope

type Json = Record<string, unknown>;
export type Envelope = { ok: boolean; operation?: string; data?: Json; error?: string };

function isRecord(v: unknown): v is Json {
	return typeof v === "object" && v !== null && !Array.isArray(v);
}

function stringsOf(v: unknown): string[] {
	return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

function recordsOf(v: unknown): Json[] {
	return Array.isArray(v) ? v.filter(isRecord) : [];
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

/** Decode `{ok, operation, data|error}`; a killed process or an exit/ok mismatch is a failure. */
export function decode(result: ExecResult, operation: string): Envelope {
	if (result.killed) return { ok: false, operation, error: "omp-evolve was interrupted (timeout or cancel)" };
	const text = result.stdout.trim();
	let value: unknown;
	for (const candidate of [text, text.split("\n").filter(l => l.trim()).pop() ?? ""]) {
		try {
			value = JSON.parse(candidate);
			break;
		} catch {
			value = undefined;
		}
	}
	if (!isRecord(value) || typeof value.ok !== "boolean") {
		return { ok: false, operation, error: `invalid omp-evolve response (exit ${result.code})` };
	}
	if (result.code !== 0 && value.ok) return { ok: false, operation, error: `omp-evolve exit ${result.code} with an ok envelope` };
	return {
		ok: value.ok,
		operation: typeof value.operation === "string" ? value.operation : operation,
		data: isRecord(value.data) ? value.data : undefined,
		error: value.ok ? undefined : typeof value.error === "string" ? value.error : "unspecified failure",
	};
}

// ---------------------------------------------------------------------------------------------
// Pure trace helpers (DESIGN §3)

export const LIMITS = { inputText: 16 * 1024, toolInput: 8 * 1024, contentHead: 1024, deliverable: 2 * 1024 };

export function head(s: string, n: number): string {
	return s.length <= n ? s : s.slice(0, n);
}

/** Canonical JSON: object keys sorted recursively; `undefined`/functions dropped as JSON.stringify does. */
export function canonicalJson(v: unknown): string {
	if (v === null || typeof v !== "object") return JSON.stringify(v) ?? "null";
	if (Array.isArray(v)) {
		return `[${v.map(x => (x === undefined || typeof x === "function" ? "null" : canonicalJson(x))).join(",")}]`;
	}
	const obj = v as Json;
	const keys = Object.keys(obj)
		.filter(k => obj[k] !== undefined && typeof obj[k] !== "function")
		.sort();
	return `{${keys.map(k => `${JSON.stringify(k)}:${canonicalJson(obj[k])}`).join(",")}}`;
}

export function argsHash(input: unknown): string {
	return createHash("sha256").update(canonicalJson(input ?? null)).digest("hex");
}

function truncStr(s: string, limit: number): string {
	return s.length <= limit ? s : `${s.slice(0, limit)}…[+${s.length - limit}]`;
}

function mapStrings(v: unknown, limit: number, maxItems: number): unknown {
	if (typeof v === "string") return truncStr(v, limit);
	if (Array.isArray(v)) {
		const kept: unknown[] = v.slice(0, maxItems).map(x => mapStrings(x, limit, maxItems));
		if (v.length > maxItems) kept.push(`…[+${v.length - maxItems}]`);
		return kept;
	}
	if (!isRecord(v)) return v;
	const out: Json = {};
	for (const [k, x] of Object.entries(v)) out[k] = mapStrings(x, limit, maxItems);
	return out;
}

function jsonBytes(v: unknown): number {
	return Buffer.byteLength(JSON.stringify(v) ?? "null", "utf8");
}

/**
 * Tool input for the trace: ≤ `max` bytes of JSON, keys kept, long strings cut with `…[+N]`
 * (N = characters removed). Shrinks the per-string limit, then long arrays, then collapses values.
 */
export function truncateToolInput(input: unknown, max = LIMITS.toolInput): unknown {
	if (input === undefined) return null;
	if (jsonBytes(input) <= max) return input;
	for (let limit = 2048; limit >= 16; limit = Math.floor(limit / 2)) {
		const out = mapStrings(input, limit, Number.MAX_SAFE_INTEGER);
		if (jsonBytes(out) <= max) return out;
	}
	for (let items = 32; items >= 1; items = Math.floor(items / 2)) {
		const out = mapStrings(input, 16, items);
		if (jsonBytes(out) <= max) return out;
	}
	if (isRecord(input)) {
		const collapsed: Json = {};
		for (const [k, x] of Object.entries(input)) collapsed[k] = typeof x === "string" ? truncStr(x, 16) : `…[+${jsonBytes(x)}]`;
		if (jsonBytes(collapsed) <= max) return collapsed;
	}
	return `…[+${jsonBytes(input)}]`;
}

const READ_SELECTOR = /(?::(?:raw|img|conflicts|-?\d+(?:[-+]\d*)?(?:,\d+(?:[-+]\d*)?)*-?))+$/;

/** Strip `read` selectors (`:50-80`, `:raw`, `:-5`, `:2-4:raw`, …) from a path. */
export function stripSelectors(p: string): string {
	if (p.startsWith("skill://")) return `skill://${p.slice(8).replace(READ_SELECTOR, "")}`;
	return p.replace(READ_SELECTOR, "");
}

/** Skill name from a `read` path: `skill://<name>[/…]` or `…/skills/<name>/SKILL.md`. */
export function skillFromReadPath(p: unknown): string | null {
	if (typeof p !== "string" || !p) return null;
	const clean = stripSelectors(p.trim());
	if (clean.startsWith("skill://")) return clean.slice(8).split(/[/?#:]/)[0] || null;
	const m = /(?:^|[\\/])skills[\\/]([^\\/]+)[\\/]SKILL\.md$/.exec(clean);
	return m ? m[1] : null;
}

export function skillsInvoked(text: string): string[] {
	const names = new Set<string>();
	for (const m of text.matchAll(/(?:^|\s)\/skill:([A-Za-z0-9][A-Za-z0-9._-]*)/g)) names.add(m[1]);
	return [...names];
}

// Prelabel regexes: verbatim with evolve-core `redirect.rs`.
export const REVERT = /\b(?:revert|undo|roll ?back|put it back|git restore)\b|\bgit checkout --(?:\s|$)/i;
export const NEGATION = /^(?:no|nope|don't|do not|stop|not that|wrong|that's (?:not|wrong)|that is wrong|this is wrong|incorrect|you didn't|you did not|not what i|i said|actually|instead)\b/i;
export const MARK = /^\/evolve\s+mark\s+(correction|preference|scope)\b/i;
export const REPROMPT_JACCARD = 0.6;

export function tokens(text: string): Set<string> {
	return new Set(text.toLowerCase().match(/[a-z0-9]+/g) ?? []);
}

export function jaccard(a: Set<string>, b: Set<string>): number {
	if (a.size === 0 && b.size === 0) return 0;
	let inter = 0;
	for (const t of a) if (b.has(t)) inter++;
	return inter / (a.size + b.size - inter);
}

/** Precedence: mark > interrupt > revert > negation > reprompt > none. */
export function prelabel(text: string, idle: boolean, previous: string | null): string {
	const mark = MARK.exec(text.trimStart());
	if (mark) return `mark:${mark[1].toLowerCase()}`;
	if (!idle) return "interrupt";
	if (REVERT.test(text)) return "revert";
	if (NEGATION.test(text.trimStart())) return "negation";
	if (previous !== null && jaccard(tokens(text), tokens(previous)) >= REPROMPT_JACCARD) return "reprompt";
	return "none";
}

/** Tool-error cluster: ≥3 errors within any window of 6 consecutive tool results. */
export class ErrorWindow {
	#window: boolean[] = [];
	push(isError: boolean): boolean {
		this.#window.push(isError);
		if (this.#window.length > 6) this.#window.shift();
		return this.#window.filter(Boolean).length >= 3;
	}
}

type ModelRef = { provider: string; id: string };

/** `provider/id[:thinking]` selector → `{provider, id}`. */
export function parseSelector(sel: unknown): ModelRef | null {
	if (typeof sel !== "string" || !sel) return null;
	const clean = sel.replace(/:(?:off|minimal|low|medium|high|xhigh|max)$/i, "");
	const slash = clean.indexOf("/");
	return slash < 0 ? { provider: "", id: clean } : { provider: clean.slice(0, slash), id: clean.slice(slash + 1) };
}

function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	return recordsOf(content)
		.filter(b => b.type === "text" && typeof b.text === "string")
		.map(b => b.text as string)
		.join("\n");
}

function contentBytes(content: unknown): number {
	if (typeof content === "string") return Buffer.byteLength(content, "utf8");
	let n = 0;
	for (const b of recordsOf(content)) {
		if (b.type === "text" && typeof b.text === "string") n += Buffer.byteLength(b.text, "utf8");
		else if (typeof b.data === "string") n += b.data.length;
	}
	return n;
}

/** Tiny POSIX-ish shell-words splitter: whitespace, '…', "…", backslash escapes. */
export function shellWords(input: string): string[] {
	const words: string[] = [];
	let cur = "";
	let started = false;
	let quote: "'" | '"' | null = null;
	for (let i = 0; i < input.length; i++) {
		const c = input[i];
		if (quote === "'") {
			if (c === "'") quote = null;
			else cur += c;
		} else if (quote === '"') {
			if (c === '"') quote = null;
			else if (c === "\\" && i + 1 < input.length && '"\\$`'.includes(input[i + 1])) cur += input[++i];
			else cur += c;
		} else if (c === "'" || c === '"') {
			quote = c;
			started = true;
		} else if (c === "\\" && i + 1 < input.length) {
			cur += input[++i];
			started = true;
		} else if (/\s/.test(c)) {
			if (started) words.push(cur);
			cur = "";
			started = false;
		} else {
			cur += c;
			started = true;
		}
	}
	if (quote) throw new Error(`unterminated ${quote} quote`);
	if (started) words.push(cur);
	return words;
}

// ---------------------------------------------------------------------------------------------
// Tool / command argv

export const OPERATIONS = [
	"doctor", "import", "reflect", "match", "digest", "record", "create", "improve", "activate", "run",
	"status", "library", "why", "task", "transfer", "ship", "count", "mark", "off", "on", "uninstall",
] as const;
export type Operation = (typeof OPERATIONS)[number];

const LONG_OPS: Record<string, true> = { reflect: true, create: true, improve: true, run: true, ship: true, import: true };
const READ_OPS: Record<string, true> = { doctor: true, status: true, digest: true, match: true, why: true };

/** `read` only for operations that cannot mutate state; everything else asks for `exec`. */
export function approvalFor(operation: string, args: readonly string[] = []): "read" | "exec" {
	if (READ_OPS[operation]) return "read";
	if (operation === "library") return args.length === 0 || args[0] === "ls" || args[0] === "why" ? "read" : "exec";
	if (operation === "transfer") return args[0] === "apply" ? "exec" : "read";
	return "exec";
}

export function timeoutFor(operation: string): number {
	return LONG_OPS[operation] ? 600_000 : 60_000;
}

export function validateArgs(args: unknown): string[] {
	if (!Array.isArray(args)) throw new Error("arguments must be an array of strings");
	for (const a of args) {
		if (typeof a !== "string" || a.includes("\0")) throw new Error("arguments must be separate strings without NUL bytes");
		if (/^--(?:model|provider|omp(?:-bin)?|editor)(?:=|$)/.test(a)) {
			throw new Error("model/provider/omp/editor overrides are not accepted through omp-evolve entry points");
		}
	}
	return args as string[];
}

function hasFlag(args: readonly string[], flag: string): boolean {
	return args.some(a => a === flag || a.startsWith(`${flag}=`));
}

/** Map an operation + arguments onto binary argv, injecting the session id / cwd it needs. */
export function buildArgv(operation: string, rawArgs: readonly string[], sessionId: string | null, cwd: string): string[] {
	if (!(OPERATIONS as readonly string[]).includes(operation)) throw new Error(`unknown omp-evolve operation: ${operation}`);
	const args = validateArgs([...rawArgs]);
	switch (operation) {
		case "why":
			return ["library", "why", ...(args.length === 0 && sessionId ? [sessionId] : args)];
		case "library":
			return ["library", ...(args.length === 0 ? ["ls"] : args)];
		case "mark": {
			const rest = [...args];
			if (rest.length > 0 && !rest[0].startsWith("-")) rest.splice(0, 1, "--class", rest[0]);
			if (!markClass(rest)) throw new Error("mark needs a class: correction|preference|scope");
			if (!hasFlag(rest, "--session")) {
				if (!sessionId) throw new Error("mark needs a session id");
				rest.unshift("--session", sessionId);
			}
			return ["mark", ...rest];
		}
		case "match":
		case "digest":
			return [operation, ...(hasFlag(args, "--cwd") ? args : ["--cwd", cwd, ...args])];
		case "record":
			return ["record", ...(hasFlag(args, "--session") || !sessionId ? args : ["--session", sessionId, ...args])];
		default:
			return [operation, ...args];
	}
}

/** Class of a `mark` request: `correction` or `--class correction`. */
export function markClass(args: readonly string[]): string | null {
	const i = args.indexOf("--class");
	const c = (i >= 0 ? args[i + 1] : args.find(a => !a.startsWith("-"))) ?? "";
	const m = /^(correction|preference|scope)$/i.exec(c);
	return m ? m[1].toLowerCase() : null;
}

export const COMMANDS = [
	"why", "status", "library", "create", "improve", "run", "activate", "mark", "off", "on", "disable",
	"rollback", "approve", "decline", "ship", "transfer", "digest", "doctor", "uninstall",
] as const;

/** `/evolve <sub> …` → (operation, arguments). `disable|rollback|approve|decline` live under `library`. */
export function commandToOperation(words: readonly string[]): { operation: Operation; args: string[] } {
	const [sub, ...rest] = words;
	if (!sub) return { operation: "library", args: ["ls"] };
	if (!(COMMANDS as readonly string[]).includes(sub)) {
		throw new Error(`unknown /evolve subcommand "${sub}"; one of ${COMMANDS.join("|")}`);
	}
	if (sub === "disable" || sub === "rollback" || sub === "approve" || sub === "decline") {
		return { operation: "library", args: [sub, ...rest] };
	}
	return { operation: sub as Operation, args: rest };
}

// ---------------------------------------------------------------------------------------------
// Module-level, read-only caches (factories are rebound per subagent; per-session state stays in the closure)

const matchCache = new Map<string, Promise<Envelope>>();

type Hook = { id: string; tool: string; test: (input: unknown) => unknown };

async function loadHook(file: string): Promise<Hook | null> {
	// Runtime-selected module: hook paths come from the binary's `match` result.
	const mod: unknown = await import(pathToFileURL(file).href);
	const h = isRecord(mod) ? mod.default : undefined;
	if (!isRecord(h) || typeof h.tool !== "string" || typeof h.test !== "function") return null;
	const id = typeof h.id === "string" && h.id ? h.id : path.basename(file).replace(/\.[cm]?[jt]s$/, "");
	const test = h.test as (input: unknown) => unknown;
	return { id, tool: h.tool, test: input => test.call(h, input) };
}

type RegistryHandle = { get(scope: unknown): unknown; override(scope: unknown, value: unknown): void };

/** Additively override `skills.customDirectories` for this process (DESIGN §0, §11.1). */
async function addSkillDirs(pi: ExtensionAPI, dirs: readonly string[]): Promise<void> {
	// Dynamic so the module still evaluates (and traces) where the host subpath is unavailable.
	const registry = await import("@oh-my-pi/pi-coding-agent/config/registry");
	const handle = registry.lookup("skills.customDirectories") as RegistryHandle | undefined;
	if (!handle) throw new Error("setting skills.customDirectories not found");
	const scope = pi.pi.settings;
	const configured = stringsOf(handle.get(scope));
	handle.override(scope, [...new Set([...configured, ...dirs])]);
}

// ---------------------------------------------------------------------------------------------
// Factory

type SessionTrace = {
	id: string;
	file: string;
	seq: number;
	lastInput: string | null;
	lastDeliverable: string | null;
	errors: ErrorWindow;
	reasons: Set<string>;
	turnsSinceAgentEnd: number;
	lastModel: ModelRef | null;
	dirReady: boolean;
};

export default async function ompEvolve(pi: ExtensionAPI): Promise<void> {
	const root = evolveRoot();
	const hardOff = process.env.OMP_EVOLVE === "0";
	let enabled = !hardOff && !fs.existsSync(path.join(root, "off"));
	const bin = resolveBinary();

	const log = (level: "warn" | "debug", msg: string, extra?: Json) => {
		try {
			pi.logger[level](`omp-evolve: ${msg}`, extra);
		} catch {
			// logging must never throw
		}
	};

	async function call(argv: string[], opts: { timeout?: number; signal?: AbortSignal } = {}): Promise<Envelope> {
		const operation = argv[0] ?? "";
		if (!bin) return { ok: false, operation, error: "omp-evolve binary not found ($OMP_EVOLVE_BIN, target/{release,debug}, PATH)" };
		try {
			const result = await pi.exec(bin, argv, { timeout: opts.timeout ?? timeoutFor(operation), signal: opts.signal });
			const value = decode(result, operation);
			if (!value.ok) log("warn", `${operation} failed: ${value.error}`, { stderr: head(result.stderr, 600) });
			return value;
		} catch (error) {
			return { ok: false, operation, error: errorText(error) };
		}
	}

	// --- load time: staging dirs + hooks ---------------------------------------------------------
	let loadedEntries: string[] = [];
	let stagingDirs: string[] = [];
	let hooks: Hook[] = [];
	if (enabled && bin) {
		try {
			const cwd = process.cwd();
			const key = `${bin}\0${cwd}`;
			let pending = matchCache.get(key);
			if (!pending) {
				pending = call(["match", "--cwd", cwd], { timeout: 5000 });
				matchCache.set(key, pending);
			}
			const match = await pending;
			if (!match.ok) throw new Error(match.error ?? "match failed");
			const data = match.data ?? {};
			if (data.off === true) {
				enabled = false;
			} else {
				loadedEntries = recordsOf(data.entries)
					.filter(e => typeof e.id === "string" && e.predicate_hit !== false)
					.map(e => e.id as string);
				const dirs = stringsOf(data.skill_dirs);
				if (dirs.length > 0) {
					await addSkillDirs(pi, dirs);
					stagingDirs = dirs;
				}
				for (const file of stringsOf(data.hooks)) {
					try {
						const hook = await loadHook(file);
						if (hook) hooks.push(hook);
						else log("warn", `hook ${file} has no {tool, test} default export; skipped`);
					} catch (error) {
						log("warn", `hook ${file} failed to import: ${errorText(error)}`);
					}
				}
			}
		} catch (error) {
			loadedEntries = [];
			stagingDirs = [];
			hooks = [];
			log("warn", `match/staging skipped: ${errorText(error)}`);
		}
	} else if (enabled) {
		log("warn", "binary not found; tracing without staging");
	}

	// --- per-session trace state (closure: one binding = one session) ----------------------------
	let trace: SessionTrace | null = null;
	let loadedAnnotated = false;

	// `agent` is absent on hosts older than the identity API: treat those as main.
	const isMain = (ctx: ExtensionContext) => ctx.agent?.kind !== "sub";
	const sessionIdOf = (ctx: ExtensionContext): string | null => {
		try {
			return ctx.sessionManager.getSessionId() || null;
		} catch {
			return null;
		}
	};
	const modelOf = (ctx: ExtensionContext): ModelRef | null => (ctx.model ? { provider: ctx.model.provider, id: ctx.model.id } : null);

	function write(ev: string, fields: Json): number | null {
		const t = trace;
		if (!t) return null;
		try {
			if (!t.dirReady) {
				fs.mkdirSync(path.dirname(t.file), { recursive: true });
				t.dirReady = true;
			}
			const seq = t.seq;
			fs.appendFileSync(t.file, `${JSON.stringify({ seq, t: Date.now(), ev, ...fields })}\n`);
			t.seq++;
			return seq;
		} catch (error) {
			log("warn", `trace write failed: ${errorText(error)}`);
			return null;
		}
	}

	function spawnReflect() {
		if (!bin) return;
		try {
			Bun.spawn({ cmd: [bin, "reflect", "--pending"], stdio: ["ignore", "ignore", "ignore"] }).unref();
		} catch (error) {
			log("warn", `reflect spawn failed: ${errorText(error)}`);
		}
	}

	async function gitTop(cwd: string): Promise<string | null> {
		try {
			const r = await pi.exec("git", ["rev-parse", "--show-toplevel"], { cwd, timeout: 3000 });
			const out = r.stdout.trim();
			return r.code === 0 && !r.killed && out ? out : null;
		} catch {
			return null;
		}
	}

	async function openTrace(ctx: ExtensionContext): Promise<void> {
		const id = sessionIdOf(ctx);
		if (!id) {
			log("warn", "no session id; not tracing");
			return;
		}
		trace = {
			id,
			file: path.join(root, "traces", `${id}.jsonl`),
			seq: 0,
			lastInput: null,
			lastDeliverable: null,
			errors: new ErrorWindow(),
			reasons: new Set(),
			turnsSinceAgentEnd: 0,
			lastModel: modelOf(ctx),
			dirReady: false,
		};
		const repo = await gitTop(ctx.cwd);
		write("session_start", {
			session_id: id,
			cwd: ctx.cwd,
			repo,
			branch_hint: path.basename(repo ?? ctx.cwd),
			model: modelOf(ctx),
			mode: ctx.mode ?? null,
			agent: { kind: ctx.agent?.kind ?? "main", name: ctx.agent?.name ?? "main" },
			omp_version: pi.pi.VERSION ?? null,
			loaded_entries: loadedEntries,
			hooks_loaded: hooks.map(h => h.id),
			staging_dirs: stagingDirs,
		});
		if (!loadedAnnotated) {
			loadedAnnotated = true;
			try {
				pi.appendEntry("com.vncp.omp-evolve.loaded", { entries: loadedEntries, hooks: hooks.map(h => h.id) });
			} catch (error) {
				log("debug", `appendEntry loaded failed: ${errorText(error)}`);
			}
		}
	}

	/** Write `session_shutdown`; if flagged, write `pending/<id>.json` synchronously (2 s budget). */
	function closeTrace(): void {
		const t = trace;
		if (!t) return;
		const reasons = [...t.reasons];
		const flagged = reasons.length > 0;
		write("session_shutdown", { reasons, flagged });
		if (flagged) {
			try {
				const dir = path.join(root, "pending");
				fs.mkdirSync(dir, { recursive: true });
				fs.writeFileSync(
					path.join(dir, `${t.id}.json`),
					JSON.stringify({ session_id: t.id, flagged_at: Date.now(), reasons, trace: t.file }),
				);
			} catch (error) {
				log("warn", `pending flag write failed: ${errorText(error)}`);
			}
		}
		trace = null;
	}

	async function presentDigest(ctx: ExtensionContext, env: Envelope): Promise<void> {
		const lines = stringsOf(env.data?.lines);
		for (const line of lines.slice(0, 5)) ctx.ui.notify(`omp-evolve: ${line}`, "info");
		if (lines.length > 5) ctx.ui.notify(`omp-evolve: +${lines.length - 5} more (/evolve digest)`, "info");
		for (const a of recordsOf(env.data?.approvals)) {
			if (typeof a.id !== "string") continue;
			const name = typeof a.name === "string" ? a.name : a.id;
			const kind = typeof a.kind === "string" ? a.kind : "entry";
			const body = [a.reason, a.diff].filter((x): x is string => typeof x === "string" && x.length > 0).join("\n\n");
			const yes = await ctx.ui.confirm(`omp-evolve: approve ${kind} ${name}?`, body);
			const r = await call(["library", yes ? "approve" : "decline", a.id]);
			ctx.ui.notify(`omp-evolve: ${yes ? "approved" : "declined"} ${name}${r.ok ? "" : ` (failed: ${r.error})`}`, r.ok ? "info" : "warning");
		}
	}

	async function showDigest(ctx: ExtensionContext, quietWhenEmpty: boolean): Promise<void> {
		if (!ctx.hasUI) return;
		const env = await call(["digest", "--cwd", ctx.cwd], { timeout: 10_000 });
		if (!env.ok) {
			if (!quietWhenEmpty) ctx.ui.notify(`omp-evolve: digest failed: ${env.error}`, "warning");
			return;
		}
		const empty = stringsOf(env.data?.lines).length === 0 && recordsOf(env.data?.approvals).length === 0;
		if (empty && !quietWhenEmpty) ctx.ui.notify("omp-evolve: nothing new", "info");
		await presentDigest(ctx, env);
	}

	async function startSession(ctx: ExtensionContext, withDigest: boolean): Promise<void> {
		await openTrace(ctx);
		spawnReflect();
		// Dialogs may outlive the handler budget: present detached, never throwing.
		if (withDigest) void showDigest(ctx, true).catch(error => log("warn", `digest failed: ${errorText(error)}`));
	}

	/** Event handlers fail open: tracing must never disturb the session. */
	function guard<E>(name: string, fn: (event: E, ctx: ExtensionContext) => unknown) {
		return async (event: E, ctx: ExtensionContext): Promise<void> => {
			try {
				await fn(event, ctx);
			} catch (error) {
				log("warn", `${name} handler failed: ${errorText(error)}`);
			}
		};
	}

	// --- tool + command (registered even when off, so `/evolve on` works) --------------------------
	function recordMark(cls: string | null) {
		if (!cls || !enabled || !trace) return;
		write("mark", { class: cls });
		trace.reasons.add(`mark:${cls}`);
	}

	async function toggle(on: boolean, ctx: ExtensionContext | undefined): Promise<Envelope> {
		const operation = on ? "on" : "off";
		if (on && hardOff) return { ok: false, operation, error: "OMP_EVOLVE=0 is set in the environment; unset it and restart omp" };
		let env = await call([operation]);
		if (!env.ok && !bin) {
			// The kill switch must work without the binary: flip the marker ourselves.
			try {
				const marker = path.join(root, "off");
				if (on) fs.rmSync(marker, { force: true });
				else {
					fs.mkdirSync(root, { recursive: true });
					fs.writeFileSync(marker, "");
				}
				env = { ok: true, operation, data: { off: !on, marker } };
			} catch (error) {
				env = { ok: false, operation, error: errorText(error) };
			}
		}
		if (env.ok) {
			enabled = on;
			if (on) {
				env.data = { ...env.data, note: "tracing resumes now; staging dirs apply from the next omp start" };
				if (!trace && ctx && isMain(ctx)) await startSession(ctx, false);
			}
		}
		return env;
	}

	async function runOperation(operation: string, args: readonly string[], ctx: ExtensionContext | undefined, signal?: AbortSignal) {
		const sid = trace?.id ?? (ctx ? sessionIdOf(ctx) : null);
		const argv = buildArgv(operation, args, sid, ctx?.cwd ?? process.cwd());
		if (operation === "off" || operation === "on") return toggle(operation === "on", ctx);
		if (operation === "mark") recordMark(markClass(argv));
		return call(argv, { signal, timeout: timeoutFor(operation) });
	}

	const z = pi.zod;
	pi.registerTool({
		name: "omp_evolve",
		label: "omp-evolve",
		description:
			"omp-evolve control plane: traces sessions, stages provisional skills/scripts/hooks grounded in traces, keeps or retires them by counters. " +
			"operation = binary subcommand (doctor, status, library, why, digest, match, create, improve, activate, run, transfer, ship, mark, off, on, …); " +
			'arguments = the remaining argv as separate strings, e.g. {"operation":"library","arguments":["ls"]}. Long work returns a job id; poll with status. ' +
			"Approving scripts/hooks and uninstalling are reserved to the user (/evolve or the session-start digest).",
		parameters: z.object({ operation: z.enum(OPERATIONS), arguments: z.array(z.string()).optional() }),
		approval: (params: unknown) => {
			const p = isRecord(params) ? params : {};
			return approvalFor(typeof p.operation === "string" ? p.operation : "", stringsOf(p.arguments));
		},
		async execute(_id, rawParams, signal, _onUpdate, ctx) {
			// The host validated the call against `parameters` before execute.
			const params = rawParams as { operation: Operation; arguments?: string[] };
			let value: Envelope;
			try {
				const args = params.arguments ?? [];
				if (params.operation === "library" && ["approve", "decline", "pin", "unpin"].includes(args[0])) {
					throw new Error(`library ${args[0]} is reserved to the user: /evolve ${args[0]} <id> or the session-start digest`);
				}
				if (params.operation === "uninstall") throw new Error("uninstall is reserved to the user: /evolve uninstall");
				value = await runOperation(params.operation, args, ctx, signal);
			} catch (error) {
				value = { ok: false, operation: params.operation, error: errorText(error) };
			}
			return { content: [{ type: "text", text: JSON.stringify(value) }], details: value, isError: value.ok !== true };
		},
	});

	pi.registerCommand("evolve", {
		description: `omp-evolve: /evolve ${COMMANDS.join("|")} [args]`,
		handler: async (raw, ctx) => {
			let value: Envelope;
			try {
				const { operation, args } = commandToOperation(shellWords(raw ?? ""));
				if (!enabled && !["on", "status", "doctor", "library", "why"].includes(operation)) {
					throw new Error(hardOff ? "omp-evolve is off (OMP_EVOLVE=0)" : "omp-evolve is off; /evolve on to enable");
				}
				let finalArgs = args;
				if (operation === "uninstall" && !args.includes("--yes")) {
					const ok = ctx.hasUI && (await ctx.ui.confirm("omp-evolve: uninstall?", "Removes every file omp-evolve wrote, nothing else."));
					if (!ok) throw new Error("uninstall not confirmed (run /evolve uninstall --yes)");
					finalArgs = [...args, "--yes"];
				}
				value = await runOperation(operation, finalArgs, ctx);
			} catch (error) {
				value = { ok: false, error: errorText(error) };
			}
			const lines = stringsOf(value.ok ? value.data?.lines : undefined);
			const content = lines.length > 0 ? lines.join("\n") : JSON.stringify(value, null, 2);
			try {
				pi.sendMessage({ customType: "omp-evolve", content, display: true, details: value });
			} catch (error) {
				log("warn", `sendMessage failed: ${errorText(error)}`);
			}
		},
	});

	if (hardOff) return;

	// Registering a shortcut also makes the TUI load this extension eagerly (DESIGN §0).
	pi.registerShortcut("alt+e", {
		description: "omp-evolve: show the digest",
		handler: async ctx => {
			try {
				await showDigest(ctx, false);
			} catch (error) {
				log("warn", `shortcut failed: ${errorText(error)}`);
			}
		},
	});

	// --- hooks: fail closed ----------------------------------------------------------------------
	pi.on("tool_call", (event, ctx) => {
		if (!enabled || hooks.length === 0) return undefined;
		for (const hook of hooks) {
			if (event.toolName !== hook.tool) continue;
			let reason: string | null;
			try {
				const r = hook.test(event.input);
				reason = typeof r === "string" && r ? r : null;
			} catch (error) {
				reason = `omp-evolve hook ${hook.id} failed: ${errorText(error)}`;
			}
			if (!reason) continue;
			if (isMain(ctx)) write("tool_blocked", { tool: event.toolName, call_id: event.toolCallId, hook_id: hook.id, reason });
			return { block: true, reason };
		}
		return undefined;
	});

	// --- trace events: fail open -----------------------------------------------------------------
	pi.on(
		"session_start",
		guard("session_start", async (_event, ctx) => {
			if (enabled && isMain(ctx)) await startSession(ctx, true);
		}),
	);

	const rotate = guard("session_rotate", async (_event: unknown, ctx) => {
		if (!enabled || !isMain(ctx)) return;
		if (trace && trace.id === sessionIdOf(ctx)) return;
		closeTrace();
		await startSession(ctx, false);
	});
	pi.on("session_switch", rotate);
	pi.on("session_branch", rotate);

	pi.on(
		"input",
		guard("input", (event: { text: string; source: string }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const text = typeof event.text === "string" ? event.text : "";
			const idle = ctx.isIdle();
			const label = prelabel(text, idle, trace.lastInput);
			// Slash commands and `!`/`%` local executions are not user turns; only `/evolve mark`
			// (prelabel `mark:<class>`) and `/skill:<name>` invocations carry signal for the trace.
			const trimmed = text.trimStart();
			if (/^[/!%]/.test(trimmed) && !label.startsWith("mark:") && !/^\/skill:/i.test(trimmed)) return;
			const seq = write("input", {
				text: head(text, LIMITS.inputText),
				source: event.source ?? null,
				idle,
				after_deliverable: trace.lastDeliverable === null ? null : head(trace.lastDeliverable, LIMITS.deliverable),
				prelabel: label,
				skills_invoked: skillsInvoked(text),
			});
			trace.lastInput = text;
			if (label === "none") return;
			trace.reasons.add(label.startsWith("mark:") ? label : `redirect:${label}`);
			try {
				pi.appendEntry("com.vncp.omp-evolve.redirect", { seq, prelabel: label, text_head: head(text, 200) });
			} catch (error) {
				log("debug", `appendEntry redirect failed: ${errorText(error)}`);
			}
		}),
	);

	pi.on(
		"tool_result",
		guard("tool_result", (event: { toolName: string; toolCallId: string; input: unknown; content: unknown; isError: boolean }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const input = event.input ?? null;
			const rawPath = isRecord(input) ? (input.path ?? input.file_path) : undefined;
			const isError = event.isError === true;
			write("tool_result", {
				tool: event.toolName,
				call_id: event.toolCallId,
				input: truncateToolInput(input),
				args_hash: argsHash(input),
				result_bytes: contentBytes(event.content),
				is_error: isError,
				error_class: isError ? "error" : null,
				content_head: head(textOf(event.content), LIMITS.contentHead),
				skill_read: event.toolName === "read" ? skillFromReadPath(rawPath) : null,
				path: typeof rawPath === "string" ? stripSelectors(rawPath) : null,
			});
			if (trace.errors.push(isError)) trace.reasons.add("tool_error_cluster");
		}),
	);

	pi.on(
		"turn_end",
		guard("turn_end", (event: { message: unknown }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const msg = isRecord(event.message) ? event.message : null;
			if (!msg || msg.role !== "assistant") return;
			const model = typeof msg.provider === "string" && typeof msg.model === "string" ? { provider: msg.provider, id: msg.model } : null;
			const last = trace.lastModel;
			if (model && last && (model.provider !== last.provider || model.id !== last.id)) {
				write("model_change", { from: last, to: model, reason: "turn" });
			}
			if (model) trace.lastModel = model;
			const u = isRecord(msg.usage) ? msg.usage : {};
			const text = textOf(msg.content);
			write("turn_end", {
				model,
				usage: {
					input: u.input ?? 0,
					output: u.output ?? 0,
					cacheRead: u.cacheRead ?? 0,
					cacheWrite: u.cacheWrite ?? 0,
					totalTokens: u.totalTokens ?? 0,
					cost: u.cost ?? null,
				},
				stop_reason: msg.stopReason ?? null,
				text_head: head(text, LIMITS.deliverable),
			});
			if (text.trim()) trace.lastDeliverable = text;
			trace.turnsSinceAgentEnd++;
		}),
	);

	pi.on(
		"retry_fallback_applied",
		guard("retry_fallback_applied", (event: { from: string; to: string; role: string; reason?: string }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const to = parseSelector(event.to);
			const why = event.reason ? `:${head(String(event.reason), 200)}` : "";
			write("model_change", { from: parseSelector(event.from), to, reason: `retry_fallback:${event.role ?? "unknown"}${why}` });
			if (to) trace.lastModel = to;
		}),
	);

	pi.on(
		"agent_end",
		guard("agent_end", (event: { messages: unknown; willContinue?: boolean }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const last = recordsOf(event.messages).findLast(m => m.role === "assistant");
			write("agent_end", {
				turns: trace.turnsSinceAgentEnd,
				is_terminal: event.willContinue !== true,
				yielded: last?.stopReason === "stop",
			});
			trace.turnsSinceAgentEnd = 0;
		}),
	);

	pi.on(
		"session_shutdown",
		guard("session_shutdown", (_event: unknown, ctx) => {
			if (enabled && isMain(ctx)) closeTrace();
		}),
	);
}
