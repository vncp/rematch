// rematch extension: traces main sessions into <root>/traces/<session_id>.jsonl (DESIGN §3),
// loads this place's skill view (`skills sync` → `skills.customDirectories`, per process) and
// withholds other places' provisional agents (`task.disabledAgents`), runs approved hooks
// (`block`/`stuck` on `tool_call`, `truncate`/`retrieve` on `context`), and exposes the `rematch`
// tool, the `/rematch` command and the `alt+e` review shortcut (DESIGN §10). Personal
// instructions reach omp only through its own `<agent_dir>/AGENTS.md` (the binary's
// `instructions` sync); nothing is appended to the system prompt at runtime.
// It parses nothing the binary owns: every decision beyond the cheap prelabels lives in Rust.
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import type { ExecResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

// ---------------------------------------------------------------------------------------------
// Paths, kill switch, binary resolution

export function rematchRoot(env: NodeJS.ProcessEnv = process.env): string {
	if (env.REMATCH_ROOT) return env.REMATCH_ROOT;
	return path.join(env.HOME || os.homedir(), ".rematch");
}

/** Logical trace ids stay unchanged in JSON; Windows filenames escape reserved characters.
 * Matches rematch-core::paths::id_file_stem. Unix layouts stay unchanged.
 */
export function idFileStem(id: string, platform: NodeJS.Platform = process.platform): string {
	if (platform !== "win32") return id;
	const chars = Array.from(id);
	const base = (id.split(".")[0] ?? "").replace(/ +$/, "");
	const device = /^(?:con|prn|aux|nul|com[0-9]|lpt[0-9])$/i.test(base);
	return chars.map((char, index) => {
		const edge = (index === 0 && (char === "." || device)) ||
			(index === chars.length - 1 && (char === "." || char === " "));
		if (/[<>:"/\\|?*\x00-\x1f%]/u.test(char) || edge) {
			return Array.from(Buffer.from(char, "utf8"), byte => `%${byte.toString(16).toUpperCase().padStart(2, "0")}`).join("");
		}
		return char;
	}).join("");
}

export function isExecutable(p: string): boolean {
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

/** The binary's file name: `rematch.exe` on Windows. */
const BIN_NAME = process.platform === "win32" ? "rematch.exe" : "rematch";

/** `$REMATCH_BIN` → `<ext>/../target/release` → `<ext>/../target/debug` → PATH. */
export function resolveBinary(env: NodeJS.ProcessEnv = process.env, extDir = extensionDir()): string | null {
	if (env.REMATCH_BIN) return isExecutable(env.REMATCH_BIN) ? env.REMATCH_BIN : null;
	for (const profile of ["release", "debug"]) {
		const candidate = path.normalize(path.join(extDir, "..", "target", profile, BIN_NAME));
		if (isExecutable(candidate)) return candidate;
	}
	for (const dir of (env.PATH ?? "").split(path.delimiter)) {
		if (!dir) continue;
		const candidate = path.join(dir, BIN_NAME);
		if (isExecutable(candidate)) return candidate;
	}
	return null;
}

// ---------------------------------------------------------------------------------------------
// Envelope

type Json = Record<string, unknown>;
export type Envelope = { ok: boolean; operation?: string; data?: Json; error?: string };

export function isRecord(v: unknown): v is Json {
	return typeof v === "object" && v !== null && !Array.isArray(v);
}

export function stringsOf(v: unknown): string[] {
	return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

export function recordsOf(v: unknown): Json[] {
	return Array.isArray(v) ? v.filter(isRecord) : [];
}

export function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

/** Decode `{ok, operation, data|error}`; a killed process or an exit/ok mismatch is a failure. */
export function decode(result: ExecResult, operation: string): Envelope {
	if (result.killed) return { ok: false, operation, error: "rematch was interrupted (timeout or cancel)" };
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
		return { ok: false, operation, error: `invalid rematch response (exit ${result.code})` };
	}
	if (result.code !== 0 && value.ok) return { ok: false, operation, error: `rematch exit ${result.code} with an ok envelope` };
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

// Changed files (ADR 001 `session_shutdown.files_changed`): verbatim with rematch-core `trace.rs`.
export const MUTATING_TOOLS = ["edit", "write", "multiedit", "notebookedit", "apply_patch", "ast_edit"];
export const PATCH_FILE = /^\["?([^\]\n"]+?)"?#[0-9A-Za-z]{4}\]\s*$|^\*\*\* (?:Add|Update|Delete) File: (.+?)\s*$|^\*\*\* Move to: (.+?)\s*$|^MV\s+"?([^"\n]+?)"?\s*$/gm;
export const FILES_CHANGED_MAX = 50;
export const FILES_CHANGED_MAX_BYTES = 1024 * 1024;
/** Share of the 2 s `session_shutdown` budget spent hashing changed files. */
export const FILES_CHANGED_BUDGET_MS = 1000;

/** Files a successful mutating tool call names: `path`/`file_path`/`paths[]`, plus the paths inside an `edit`/`apply_patch` payload. */
export function changedPaths(tool: string, input: unknown): string[] {
	const name = tool.toLowerCase();
	if (!MUTATING_TOOLS.includes(name) || !isRecord(input)) return [];
	const out: string[] = [];
	const push = (p: unknown) => {
		if (typeof p !== "string") return;
		const clean = stripSelectors(p.trim());
		if (clean && !clean.includes("://") && !out.includes(clean)) out.push(clean);
	};
	push(input.path);
	push(input.file_path);
	if (Array.isArray(input.paths)) for (const p of input.paths) push(p);
	if (name === "edit" || name === "apply_patch") {
		for (const key of ["input", "patch"]) {
			const text = input[key];
			if (typeof text !== "string") continue;
			for (const m of text.matchAll(PATCH_FILE)) push(m[1] ?? m[2] ?? m[3] ?? m[4]);
		}
	}
	return out;
}

/**
 * `[path, sha256]` of the first `FILES_CHANGED_MAX` readable regular files of `paths` (relative ones
 * resolve against `cwd`) no larger than `FILES_CHANGED_MAX_BYTES`, repo-relative inside `repo`;
 * `null` when `deadline` (ms epoch) passes first, so the record omits the field.
 */
export function filesChanged(paths: readonly string[], cwd: string, repo: string | null, deadline: number): [string, string][] | null {
	const out: [string, string][] = [];
	for (const p of paths) {
		if (out.length >= FILES_CHANGED_MAX) break;
		if (Date.now() > deadline) return null;
		const abs = path.resolve(cwd, p);
		try {
			const st = fs.statSync(abs);
			if (!st.isFile() || st.size > FILES_CHANGED_MAX_BYTES) continue;
			const sha = createHash("sha256").update(fs.readFileSync(abs)).digest("hex");
			const rel = repo ? path.relative(repo, abs) : "";
			out.push([repo && rel && !rel.startsWith("..") && !path.isAbsolute(rel) ? rel.split(path.sep).join("/") : abs, sha]);
		} catch {
			// unreadable or gone: not recorded
		}
	}
	return out;
}

/** `cwd` relative to the repo toplevel (`""` at the top), trying the cwd's real path for symlinked roots; null outside. */
export function subpathOf(top: string, cwd: string): string | null {
	for (const c of [cwd, (() => { try { return fs.realpathSync(cwd); } catch { return cwd; } })()]) {
		const rel = path.relative(top, c);
		if (!rel.startsWith("..") && !path.isAbsolute(rel)) return rel.split(path.sep).join("/");
	}
	return null;
}

export function skillsInvoked(text: string): string[] {
	const names = new Set<string>();
	for (const m of text.matchAll(/(?:^|\s)\/skill:([A-Za-z0-9][A-Za-z0-9._-]*)/g)) names.add(m[1]);
	return [...names];
}

// Prelabel regexes: verbatim with rematch-core `redirect.rs`.
export const REVERT = /\b(?:revert|undo|roll ?back|put it back|git restore)\b|\bgit checkout --(?:\s|$)/i;
export const NEGATION = /^(?:no|nope|don't|do not|stop|not that|wrong|that's (?:not|wrong)|that is wrong|this is wrong|incorrect|you didn't|you did not|not what i|i said|actually|instead)\b/i;
export const MARK = /^\/rematch\s+mark\s+(correction|preference|scope)\b/i;
export const REPROMPT_JACCARD = 0.6;
export const REPROMPT_MIN_TOKENS = 6;

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
	if (previous !== null) {
		const now = tokens(text);
		if (now.size >= REPROMPT_MIN_TOKENS && jaccard(now, tokens(previous)) >= REPROMPT_JACCARD) return "reprompt";
	}
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

export function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	return recordsOf(content)
		.filter(b => b.type === "text" && typeof b.text === "string")
		.map(b => b.text as string)
		.join("\n");
}

export function contentBytes(content: unknown): number {
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
	"status", "library", "why", "task", "transfer", "ship", "count", "mark", "off", "on", "uninstall", "builtins", "anchor",
] as const;
export type Operation = (typeof OPERATIONS)[number];

const LONG_OPS: Record<string, true> = { reflect: true, create: true, improve: true, run: true, ship: true, import: true, anchor: true };
const READ_OPS: Record<string, true> = { doctor: true, status: true, digest: true, match: true, why: true };

/**
 * The subcommand words of a binary argv, as clap reads them: the global flags (`--text`, `--cwd DIR`,
 * `--cwd=DIR`) may stand anywhere and are skipped; every other `-…` word is an option of the
 * subcommand, not a word of its path. `["library", "--cwd", "x", "--text", "approve", "id"]` →
 * `["library", "approve", "id"]`. Every word after `--` counts (the conservative reading).
 */
export function commandWords(argv: readonly string[]): string[] {
	const out: string[] = [];
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i] as string;
		if (a === "--cwd") {
			i++;
			continue;
		}
		if (a === "--") {
			out.push(...argv.slice(i + 1));
			break;
		}
		if (a.startsWith("-")) continue;
		out.push(a);
	}
	return out;
}

/**
 * Why the model may not run this binary argv, or null: approving, declining, pinning, disabling,
 * rolling back, approving exports, marking, switching rematch off or on, uninstalling and setting
 * or removing a skill's place scope (`policy scope set|unset`) are the user's, wherever global
 * flags stand. The one check every model-facing entry point (omp's and Pi's `rematch` tool, and
 * through [`userOnlyShellReason`] the shell guards of omp, Pi and OpenCode) runs on the argv it
 * would execute.
 */
export function userOnlyReason(argv: readonly string[]): string | null {
	const [cmd, sub, verb] = commandWords(argv);
	if (cmd === "off" || cmd === "on" || cmd === "uninstall" || cmd === "mark") return `${cmd} is reserved to the user: /rematch ${cmd}`;
	if (cmd === "library" && sub !== undefined && ["approve", "decline", "pin", "unpin", "disable", "rollback"].includes(sub)) {
		return `library ${sub} is reserved to the user: /rematch ${sub} <id>, /rematch review or the session-start digest`;
	}
	if (cmd === "library" && sub === "export" && argv.some(a => /^--approve(?:-all)?(?:=|$)/.test(a))) {
		return "approving an export is reserved to the user: rematch library export --approve in a shell, or /rematch review";
	}
	if (cmd === "policy" && sub === "scope" && (verb === "set" || verb === "unset")) {
		return `policy scope ${verb} is reserved to the user: rematch policy scope ${verb} in their own shell`;
	}
	return null;
}

/** Words that may stand before the command itself: runners (`sudo rematch …`, `sh rematch.sh …`). */
const COMMAND_PREFIX: Record<string, true> = { env: true, exec: true, command: true, sudo: true, nohup: true, time: true, sh: true, bash: true, zsh: true };

/**
 * Why a model's shell line may not run, or null: a `rematch` / `rematch.sh` / `rematch.exe` /
 * `rematch.cmd` word, quoted or by path, in command position of any `;`/`&&`/`|`/subshell segment
 * (after env assignments and runners such as `sudo`, `sh`, or inside the quoted script of
 * `sh -c`), whose argv [`userOnlyReason`] refuses: the same parser as the `rematch` tool, so global
 * flags anywhere change nothing. Mentions elsewhere (`git commit -m "… rematch off"`, `echo rematch
 * on`) pass. A guard against the direct command, not a sandbox: the model's shell has the user's
 * permissions, and an arbitrary script, `eval` of a built string or an alias still runs.
 */
export function userOnlyShellReason(command: unknown): string | null {
	if (typeof command !== "string") return null;
	for (const segment of command.split(/&&|\|\||[;&|\n(){}`]|\$\(/)) {
		const words = (segment.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g) ?? []).map(w => w.replace(/["']/g, ""));
		for (let i = 0; i < words.length; i++) {
			const word = words[i] as string;
			const base = (word.split(/[\\/]/).pop() ?? "").toLowerCase();
			if (/^rematch(?:\.sh|\.exe|\.cmd)?$/.test(base)) {
				const refused = userOnlyReason(words.slice(i + 1));
				if (refused) return refused;
				break;
			}
			// The quoted script of `sh -c "…"` is a command line of its own.
			if (/\s/.test(word) && i > 0) {
				const refused = userOnlyShellReason(word);
				if (refused) return refused;
				break;
			}
			if (/^[A-Za-z_]\w*=/.test(word) || word.startsWith("-") || COMMAND_PREFIX[base] === true) continue;
			break;
		}
	}
	return null;
}

/** `read` only for operations that cannot mutate state; everything else asks for `exec`. */
export function approvalFor(operation: string, args: readonly string[] = []): "read" | "exec" {
	if (READ_OPS[operation]) return "read";
	const [sub] = commandWords(args);
	if (operation === "library") return sub === undefined || ["ls", "why", "inspect"].includes(sub) ? "read" : "exec";
	if (operation === "transfer") return sub === undefined || sub === "suggest" ? "read" : "exec";
	if (operation === "builtins") return sub === "reinstall" ? "exec" : "read";
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
			throw new Error("model/provider/omp/editor overrides are not accepted through rematch entry points");
		}
	}
	return args as string[];
}

function hasFlag(args: readonly string[], flag: string): boolean {
	return args.some(a => a === flag || a.startsWith(`${flag}=`));
}

/** Map an operation + arguments onto binary argv, injecting the session id / cwd it needs. */
export function buildArgv(operation: string, rawArgs: readonly string[], sessionId: string | null, cwd: string): string[] {
	if (!(OPERATIONS as readonly string[]).includes(operation)) throw new Error(`unknown rematch operation: ${operation}`);
	const args = validateArgs([...rawArgs]);
	switch (operation) {
		case "why":
			return ["library", "why", ...(args.length === 0 && sessionId ? [sessionId] : args)];
		case "library":
			return ["library", ...(args.length === 0 ? ["ls"] : args)];
		case "builtins":
			return ["builtins", ...(args.length === 0 ? ["ls"] : args)];
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

/** Binary argv after whose success the loaded set changed (hook modules written or removed): re-match. */
export function changesLoaded(argv: readonly string[]): boolean {
	if (argv[0] === "activate") return true;
	return argv[0] === "library" && ["approve", "disable", "rollback"].includes(argv[1] ?? "");
}

export const COMMANDS = [
	"why", "status", "library", "create", "improve", "run", "activate", "mark", "off", "on", "disable",
	"rollback", "approve", "decline", "inspect", "review", "ship", "transfer", "digest", "doctor", "uninstall", "builtins", "anchor",
] as const;

/** `/rematch <sub> …` → (operation, arguments). `disable|rollback|approve|decline|inspect` live under `library`; `review` opens the review dialog (handled by the command). */
export function commandToOperation(words: readonly string[]): { operation: Operation; args: string[] } {
	const [sub, ...rest] = words;
	if (!sub) return { operation: "library", args: ["ls"] };
	if (!(COMMANDS as readonly string[]).includes(sub)) {
		throw new Error(`unknown /rematch subcommand "${sub}"; one of ${COMMANDS.join("|")}`);
	}
	if (sub === "disable" || sub === "rollback" || sub === "approve" || sub === "decline" || sub === "inspect") {
		return { operation: "library", args: [sub, ...rest] };
	}
	if (sub === "review") return { operation: "digest", args: rest };
	return { operation: sub as Operation, args: rest };
}

// ---------------------------------------------------------------------------------------------
// Review (persistent actions, `digest.actions`): nothing changes without an explicit decision.

export type ReviewAction = {
	id: string;
	kind: string;
	name: string;
	summary: string;
	state: string;
	blocked: string | null;
	reload: string | null;
	approve: string[] | null;
	decline: string[] | null;
};

/** `digest.actions` as review items (every pending, blocked and conflict action, whatever `digest.last` says). */
export function reviewActionsOf(data: unknown): ReviewAction[] {
	const argv = (v: unknown): string[] | null => {
		const a = stringsOf(v);
		return a.length > 0 && a[0] === "library" ? a : null;
	};
	return (isRecord(data) ? recordsOf(data.actions) : [])
		.filter(a => typeof a.id === "string")
		.map(a => {
			const c = isRecord(a.commands) ? a.commands : {};
			return {
				id: a.id as string,
				kind: typeof a.kind === "string" ? a.kind : "entry",
				name: typeof a.name === "string" ? a.name : (a.id as string),
				summary: typeof a.summary === "string" ? a.summary : "",
				state: typeof a.state === "string" ? a.state : "pending",
				blocked: typeof a.blocked === "string" ? a.blocked : null,
				reload: typeof a.reload === "string" ? a.reload : null,
				approve: argv(c.approve_argv),
				decline: argv(c.decline_argv),
			};
		});
}

/** Footer status: how many actions wait, and where to review them; undefined when none. */
export function pendingStatus(actions: readonly ReviewAction[]): string | undefined {
	if (actions.length === 0) return undefined;
	return `rematch: ${actions.length} waiting · /rematch review`;
}

export const REVIEW_APPROVE = "Approve";
export const REVIEW_DECLINE = "Decline";
export const REVIEW_INSPECT = "Inspect";
export const REVIEW_LATER = "Later";

/** What Approve and Decline do for an action kind (the binary's `library approve|decline` dispatch). */
export function reviewEffects(kind: string): { approve: string | null; decline: string } {
	switch (kind) {
		case "instructions":
			return { approve: null, decline: "this text is not applied; the entry stays" };
		case "instructions_applied":
			return { approve: null, decline: "withdraws this text from your personal AGENTS.md (on every synced machine when it came through the profile)" };
		case "instructions_conflict":
			return { approve: "restore rematch's text over your edit", decline: "keep your edited text as your own; rematch stops managing it" };
		case "instructions_unreadable":
			return { approve: null, decline: "withdraws the runtime approval of this text" };
		case "export":
			return { approve: null, decline: "the export is not offered again; the entry stays" };
		case "probe":
			return { approve: null, decline: "no replay runs" };
		default:
			return { approve: null, decline: "retires the entry" };
	}
}

/** The choices of one action: approve/decline only when the binary offers that exact command; Later always (and Esc means Later). */
export function reviewChoices(a: ReviewAction): { label: string; description?: string }[] {
	const out: { label: string; description?: string }[] = [];
	const effects = reviewEffects(a.kind);
	if (a.approve && !a.blocked) out.push({ label: REVIEW_APPROVE, description: effects.approve ?? a.approve.slice(1).join(" ") });
	if (a.decline) out.push({ label: REVIEW_DECLINE, description: effects.decline });
	out.push({ label: REVIEW_INSPECT, description: `rematch library inspect ${a.id}` });
	out.push({ label: REVIEW_LATER, description: "keep it pending; nothing changes" });
	return out;
}

/** The binary argv a choice runs: approve/decline exactly as offered (with the reviewed hash); null for Inspect, Later or a dismissed dialog. */
export function reviewArgv(a: ReviewAction, choice: string | undefined): string[] | null {
	if (choice === REVIEW_APPROVE && !a.blocked) return a.approve;
	if (choice === REVIEW_DECLINE) return a.decline;
	return null;
}

/**
 * `/rematch approve|decline <id>` typed without `--sha`: the id of the one action to review in a
 * dialog, whose Approve / Decline run the action's own argv (the reviewed hash with it), so a
 * typed approval needs no copied hash. Null for anything else (a typed `--sha` goes to the binary
 * as is, which checks it).
 */
export function reviewTarget(words: readonly string[]): string | null {
	const [sub, id, ...rest] = words;
	if (sub !== "approve" && sub !== "decline") return null;
	if (!id || id.startsWith("-") || rest.some(a => a === "--sha" || a.startsWith("--sha="))) return null;
	return id;
}

// ---------------------------------------------------------------------------------------------
// Hook modes (DESIGN §10). A compiled module is data only:
// `export default { id, mode, tool, field, regex, message, pass_condition, params }`; the modes live here.
// `block`/`stuck` run on `tool_call` and fail closed; `truncate`/`retrieve` run on `context` and fail open.

export const HOOK_MODES = ["block", "truncate", "stuck", "retrieve"] as const;
export type HookMode = (typeof HOOK_MODES)[number];
export type Hook = {
	id: string;
	mode: HookMode;
	tool: string;
	field: string;
	message: string;
	/** `block` only: `params.{source,flags}` as a RegExp. */
	pattern: RegExp | null;
	params: Record<string, number>;
};

const MODE_PARAMS: Record<HookMode, readonly string[]> = {
	block: [],
	truncate: ["max_age_turns", "keep_chars"],
	stuck: ["repeats"],
	retrieve: ["budget_bytes", "min_overlap"],
};

/** Validate a module's default export; throws on anything the binary would not have written. */
export function hookFromModule(def: unknown, fallbackId: string): Hook {
	if (!isRecord(def)) throw new Error("no default export object");
	const mode = def.mode;
	if (typeof mode !== "string" || !(HOOK_MODES as readonly string[]).includes(mode)) throw new Error(`unknown hook mode ${String(mode)}`);
	if (typeof def.tool !== "string" || !def.tool) throw new Error("hook names no tool");
	const params = isRecord(def.params) ? def.params : {};
	const nums: Record<string, number> = {};
	for (const key of MODE_PARAMS[mode as HookMode]) {
		const v = params[key];
		if (typeof v !== "number" || !Number.isFinite(v) || v < 0) throw new Error(`${mode} hook needs a numeric params.${key}`);
		nums[key] = v;
	}
	let pattern: RegExp | null = null;
	if (mode === "block") {
		if (typeof def.field !== "string" || !def.field) throw new Error("block hook names no field");
		if (typeof params.source !== "string" || typeof params.flags !== "string") throw new Error("block hook needs params.source and params.flags");
		pattern = new RegExp(params.source, params.flags);
	}
	return {
		id: typeof def.id === "string" && def.id ? def.id : fallbackId,
		mode: mode as HookMode,
		tool: def.tool,
		field: typeof def.field === "string" ? def.field : "",
		message: typeof def.message === "string" ? def.message : "",
		pattern,
		params: nums,
	};
}

/**
 * Import an approved hook module. The `?v=<mtime>` suffix makes a rewritten module (re-approval,
 * rollback) a new module record; Bun keys its ESM cache by the specifier and ignores the query of
 * a `file:` URL, so the suffix goes on the plain path.
 */
export async function loadHook(file: string): Promise<Hook> {
	// Runtime-selected module: hook paths come from the binary's `match` result.
	const mod: unknown = await import(`${file}?v=${fs.statSync(file).mtimeMs}`);
	return hookFromModule(isRecord(mod) ? mod.default : undefined, path.basename(file).replace(/\.[cm]?[jt]s$/, ""));
}

function toolMatches(hook: Hook, toolName: string): boolean {
	return hook.tool === "*" || hook.tool === toolName;
}

/** `block`: the rule's message when `input[field]` matches its pattern, else null. */
export function blockReason(hook: Hook, toolName: string, input: unknown): string | null {
	if (hook.mode !== "block" || !hook.pattern || !toolMatches(hook, toolName)) return null;
	const value = isRecord(input) ? input[hook.field] : undefined;
	if (typeof value !== "string") return null;
	hook.pattern.lastIndex = 0;
	return hook.pattern.test(value) ? hook.message : null;
}

export function callKey(toolName: string, input: unknown): string {
	return `${toolName}\0${argsHash(input)}`;
}

/** The session's recent tool calls (tool + args hash), most recent last. */
export class CallHistory {
	#keys: string[] = [];
	/** Calls immediately before now that were identical to `key`. */
	run(key: string): number {
		let n = 0;
		for (let i = this.#keys.length - 1; i >= 0 && this.#keys[i] === key; i--) n++;
		return n;
	}
	push(key: string): void {
		this.#keys.push(key);
		if (this.#keys.length > 32) this.#keys.shift();
	}
	clear(): void {
		this.#keys = [];
	}
}

/** `stuck`: the message once `repeats` identical calls ran right before this identical one. */
export function stuckReason(hook: Hook, toolName: string, run: number): string | null {
	if (hook.mode !== "stuck" || !toolMatches(hook, toolName)) return null;
	return run >= hook.params.repeats ? hook.message : null;
}

export function truncationMarker(removed: number, age: number): string {
	return `…[rematch: truncated ${removed} chars, age ${age} turns]`;
}

/**
 * `truncate`: a tool result older than `max_age_turns` user turns (more user messages after it)
 * keeps the first `keep_chars` of its text plus a marker; the strictest matching rule wins and
 * non-text blocks are kept. Returns a new array (untouched messages reused) or null when nothing
 * changed. Only the outgoing request is affected: `context` receives a copy (DESIGN §0).
 */
export function truncateToolResults(messages: readonly unknown[], hooks: readonly Hook[]): unknown[] | null {
	const rules = hooks.filter(h => h.mode === "truncate");
	if (rules.length === 0) return null;
	const out = [...messages];
	let changed = false;
	let age = 0;
	for (let i = out.length - 1; i >= 0; i--) {
		const m = out[i];
		if (!isRecord(m)) continue;
		if (m.role === "user" && m.synthetic !== true) {
			age++;
			continue;
		}
		if (m.role !== "toolResult" || !Array.isArray(m.content)) continue;
		const toolName = typeof m.toolName === "string" ? m.toolName : "";
		let keep = Number.POSITIVE_INFINITY;
		for (const r of rules) {
			if (toolMatches(r, toolName) && age > r.params.max_age_turns) keep = Math.min(keep, r.params.keep_chars);
		}
		if (!Number.isFinite(keep)) continue;
		const text = textOf(m.content);
		if (text.length <= keep) continue;
		// Never split a surrogate pair: a lone surrogate is invalid in the outgoing request.
		const last = text.charCodeAt(keep - 1);
		const cut = last >= 0xd800 && last <= 0xdbff ? keep - 1 : keep;
		const others = (m.content as unknown[]).filter(b => !(isRecord(b) && b.type === "text"));
		out[i] = { ...m, content: [{ type: "text", text: `${text.slice(0, cut)}\n${truncationMarker(text.length - cut, age)}` }, ...others] };
		changed = true;
	}
	return changed ? out : null;
}

/** `retrieve` query: the last user message text + the paths of the last `maxPaths` tool results. */
export function retrievalQuery(messages: readonly unknown[], maxPaths = 10): string {
	const recs = messages.filter(isRecord);
	const lastUser = recs.findLast(m => m.role === "user" && m.synthetic !== true);
	const callPaths = new Map<string, string>();
	for (const m of recs) {
		if (m.role !== "assistant") continue;
		for (const b of recordsOf(m.content)) {
			if (b.type !== "toolCall" || typeof b.id !== "string" || !isRecord(b.arguments)) continue;
			const p = b.arguments.path ?? b.arguments.file_path;
			if (typeof p === "string" && p) callPaths.set(b.id, stripSelectors(p));
		}
	}
	const paths: string[] = [];
	for (const r of recs.filter(m => m.role === "toolResult").slice(-maxPaths)) {
		const p = typeof r.toolCallId === "string" ? callPaths.get(r.toolCallId) : undefined;
		if (p && !paths.includes(p)) paths.push(p);
	}
	return head([lastUser ? textOf(lastUser.content) : "", ...paths].filter(s => s.trim()).join("\n"), LIMITS.inputText);
}

export type RetrievedUnit = { entry_id: string; name: string; path: string; title: string; text: string };

/** Units of a `retrieve` envelope's data (the binary owns ranking and the budget). */
export function retrievedUnits(data: unknown): RetrievedUnit[] {
	const units = isRecord(data) ? recordsOf(data.units) : [];
	return units
		.filter(u => ["entry_id", "name", "path", "title", "text"].every(k => typeof u[k] === "string"))
		.map(u => ({ entry_id: u.entry_id as string, name: u.name as string, path: u.path as string, title: u.title as string, text: u.text as string }));
}

export const RETRIEVAL_TYPE = "com.vncp.rematch.retrieval";

/**
 * The one message `retrieve` appends: an omp `CustomMessage` (`role: "custom"`, hidden), which the
 * provider conversion sends as a `developer` message (pi-agent-core `compaction/messages.ts`
 * `CustomMessage` + `convertMessageToLlm`).
 */
export function retrievalMessage(units: readonly RetrievedUnit[], timestamp = Date.now()): Json {
	const body = units.map(u => `<unit skill="${u.name}" path="${u.path}">\n${u.text.trimEnd()}\n</unit>`).join("\n\n");
	return {
		role: "custom",
		customType: RETRIEVAL_TYPE,
		display: false,
		timestamp,
		content: `[rematch] Units of skills loaded in this session that match the current request, retrieved by the harness (the whole skill: skill://<name>).\n\n${body}`,
		details: { units: units.map(u => ({ entry_id: u.entry_id, path: u.path })) },
	};
}

/** One versioned `session_start.loaded_entries` item (ADR 001). */
export type LoadedEntry = { id: string; version?: number; sha?: string };

/**
 * `session_start.loaded_entries` of a `match` result: entries with a predicate hit, minus the
 * `retrieve` hooks, which a session credits through its `retrieval` record only where they injected
 * units (DESIGN §7); each `{id, version, sha}` as `match` reported it.
 */
export function loadedEntriesOf(data: unknown, hooks: readonly Hook[]): LoadedEntry[] {
	const retrieveIds = new Set(hooks.filter(h => h.mode === "retrieve").map(h => h.id));
	return (isRecord(data) ? recordsOf(data.entries) : [])
		.filter(e => typeof e.id === "string" && e.predicate_hit !== false && !retrieveIds.has(e.id))
		.map(e => {
			const out: LoadedEntry = { id: e.id as string };
			if (typeof e.version === "number") out.version = e.version;
			if (typeof e.sha === "string" && e.sha) out.sha = e.sha;
			return out;
		});
}

/** One approved personal `AGENTS.fragment.md` the binary reported for this cwd (`match.prompt_fragments`); `sha` is the approved hash, recorded when a host sends it (`fragments_sent`). */
export type PromptFragment = { id: string; name: string; text: string; sha?: string };

/** `match.prompt_fragments`, keeping only well-formed items (`{id, name, text}`, all non-empty strings; `sha` kept when present). */
export function promptFragmentsOf(data: unknown): PromptFragment[] {
	return (isRecord(data) ? recordsOf(data.prompt_fragments) : [])
		.filter(f => ["id", "name", "text"].every(k => typeof f[k] === "string" && (f[k] as string).trim() !== ""))
		.map(f => {
			const out: PromptFragment = { id: f.id as string, name: f.name as string, text: f.text as string };
			if (typeof f.sha === "string" && f.sha) out.sha = f.sha;
			return out;
		});
}

/**
 * The system-prompt block of the fragments: `## rematch: <name>\n\n<text trimmed>` per fragment, the
 * heading `ship` writes into a repo's AGENTS.md, joined with a blank line; null when there is none.
 */
export function renderPromptFragments(fragments: readonly PromptFragment[]): string | null {
	if (fragments.length === 0) return null;
	return fragments.map(f => `## rematch: ${f.name}\n\n${f.text.trim()}`).join("\n\n");
}

/**
 * `before_agent_start.systemPrompt` (an array of strings on omp 18.13, a string on older hosts) with
 * `block` appended: one more array element, or the string joined by a blank line; null for any other shape.
 */
export function withPromptBlock(systemPrompt: unknown, block: string): string | string[] | null {
	if (Array.isArray(systemPrompt)) return [...systemPrompt, block];
	if (typeof systemPrompt === "string") return systemPrompt === "" ? block : `${systemPrompt}\n\n${block}`;
	return null;
}

/** `systemPrompt` with `fragments` rendered and appended; null when there is nothing to add or the prompt has an odd shape. */
export function appendPromptFragments(systemPrompt: unknown, fragments: readonly PromptFragment[]): string | string[] | null {
	const block = renderPromptFragments(fragments);
	return block ? withPromptBlock(systemPrompt, block) : null;
}

// ---------------------------------------------------------------------------------------------
// Module-level cache of `match` + `skills sync` results (factories are rebound per subagent;
// per-session state stays in the closure). Keyed by binary + cwd and cleared when the main
// session rotates or the user changes what loads, so a cached result never outlives the session
// it was matched for.

type Matched = { match: Envelope; sync: Envelope };
const matchCache = new Map<string, Promise<Matched>>();

type RegistryHandle = { get(scope: unknown): unknown; override(scope: unknown, value: unknown): void };

/** A skills directory rematch mounts: the kept library `<root>/skills` or a place view `<root>/views/<key>/skills`. */
export function isRematchSkillDir(dir: string, root: string): boolean {
	const d = path.resolve(dir);
	return d === path.resolve(root, "skills") || d.startsWith(path.resolve(root, "views") + path.sep);
}

/** `skills.customDirectories` for this session: what the user configured, minus earlier rematch dirs, plus `dirs` (this place's view). */
export function sessionSkillDirs(configured: readonly string[], dirs: readonly string[], root: string): string[] {
	return [...new Set([...configured.filter(d => !isRematchSkillDir(d, root)), ...dirs])];
}

/** `task.disabledAgents` for this process: the user's list plus the rematch agents this session must not offer. */
export function sessionDisabledAgents(configured: readonly string[], withheld: readonly string[]): string[] {
	return [...new Set([...configured, ...withheld])];
}

/** Names linked under `<root>/agents/` (`<name>.md`): every provisional agent rematch put in omp's reach. */
export function rematchAgentNames(root: string): string[] {
	try {
		return fs
			.readdirSync(path.join(root, "agents"))
			.filter(f => f.endsWith(".md") && !f.startsWith("."))
			.map(f => f.slice(0, -3))
			.sort();
	} catch {
		return [];
	}
}

/**
 * The rematch entry a successful `read` opened: `skill://<name>` resolves through this session's
 * view (`name → entry id`; omp gives a custom-directory skill the bare name over a native one of the
 * same name), a `…/<name>/SKILL.md` path through the `rematch.json` beside it. Null for a native
 * skill or any other read: no credit for a same-named skill rematch did not write.
 */
export function skillEntryOfRead(rawPath: unknown, view: ReadonlyMap<string, string>, cwd: string): string | null {
	if (typeof rawPath !== "string" || !rawPath) return null;
	const clean = stripSelectors(rawPath.trim());
	if (clean.startsWith("skill://")) {
		const name = skillFromReadPath(clean);
		return name ? (view.get(name) ?? null) : null;
	}
	if (!/(?:^|[\\/])SKILL\.md$/.test(clean)) return null;
	return entryOfSkillDir(path.dirname(path.resolve(cwd, clean)));
}

/** The entry id of a skill dir rematch wrote (its `rematch.json`), else null: a native skill. */
export function entryOfSkillDir(dir: string): string | null {
	try {
		const v: unknown = JSON.parse(fs.readFileSync(path.join(dir, "rematch.json"), "utf8"));
		return isRecord(v) && typeof v.id === "string" ? v.id : null;
	} catch {
		return null;
	}
}

/** `sync.data.agents.withheld` of a `skills sync` envelope, or null when the sync did not answer (fail safe: withhold every rematch agent). */
export function withheldAgentsOf(sync: Envelope): string[] | null {
	if (!sync.ok) return null;
	const agents = isRecord(sync.data?.agents) ? sync.data.agents : {};
	return stringsOf(agents.withheld);
}

/** Runtime-only override (never persisted) of a list setting: `value(persisted)` for this process. Returns the persisted list. */
async function overrideList(pi: ExtensionAPI, id: string, value: (persisted: string[]) => string[]): Promise<string[]> {
	// Dynamic so the module still evaluates (and traces) where the host subpath is unavailable.
	const registry = await import("@oh-my-pi/pi-coding-agent/config/registry");
	const handle = registry.lookup(id) as RegistryHandle | undefined;
	if (!handle) throw new Error(`setting ${id} not found`);
	const scope = pi.pi.settings;
	handle.override(scope, undefined);
	const persisted = stringsOf(handle.get(scope));
	handle.override(scope, value(persisted));
	return persisted;
}

/**
 * Names of `view` that a user-configured custom skill dir also holds: omp keeps the bare name for
 * the first custom dir listed (the user's come first), so `skill://<name>` opens the user's skill
 * and a read is no use of rematch's.
 */
export function shadowedByCustomDirs(view: ReadonlyMap<string, string>, customDirs: readonly string[], root: string): string[] {
	const dirs = customDirs.filter(d => !isRematchSkillDir(d, root));
	return [...view.keys()].filter(name => dirs.some(d => fs.existsSync(path.join(d.replace(/^~(?=$|[\\/])/, os.homedir()), name, "SKILL.md"))));
}

// ---------------------------------------------------------------------------------------------
// Factory

type SessionTrace = {
	id: string;
	file: string;
	seq: number;
	lastInput: string | null;
	lastDeliverable: string | null;
	/** True between `agent_start` and `agent_end`: an input then is a takeover, not a prompt. */
	agentRunning: boolean;
	errors: ErrorWindow;
	reasons: Set<string>;
	turnsSinceAgentEnd: number;
	lastModel: ModelRef | null;
	dirReady: boolean;
	cwd: string;
	repo: string | null;
	/** Files successful mutating tool calls named, hashed into `session_shutdown.files_changed`. */
	changed: Set<string>;
};

export default async function rematch(pi: ExtensionAPI): Promise<void> {
	const root = rematchRoot();
	const hardOff = process.env.REMATCH === "0";
	let enabled = !hardOff && !fs.existsSync(path.join(root, "off"));
	const bin = resolveBinary();

	const log = (level: "warn" | "debug", msg: string, extra?: Json) => {
		try {
			pi.logger[level](`rematch: ${msg}`, extra);
		} catch {
			// logging must never throw
		}
	};

	async function call(argv: string[], opts: { timeout?: number; signal?: AbortSignal } = {}): Promise<Envelope> {
		const operation = argv[0] ?? "";
		if (!bin) return { ok: false, operation, error: "rematch binary not found ($REMATCH_BIN, target/{release,debug}, PATH)" };
		try {
			const result = await pi.exec(bin, argv, { timeout: opts.timeout ?? timeoutFor(operation), signal: opts.signal });
			const value = decode(result, operation);
			if (!value.ok) log("warn", `${operation} failed: ${value.error}`, { stderr: head(result.stderr, 600) });
			return value;
		} catch (error) {
			return { ok: false, operation, error: errorText(error) };
		}
	}

	// --- loaded state: skill dirs + hooks matched for `loadedCwd` --------------------------------
	// Matched at load time for process.cwd(); re-matched when a main session's cwd changes, when the
	// main session switches or branches, and after the user approves, activates, disables or rolls
	// back an entry, so approved hooks run at once. Skills load from this place's view (`skills sync`
	// `dirs`: the kept library plus this place's provisional entries), mounted for this process only;
	// omp scans it when a session starts. Provisional agents of other places are disabled for this
	// process (`task.disabledAgents`), every one of them when the binary cannot say (fail safe).
	let loadedCwd: string | null = null;
	let loadedEntries: LoadedEntry[] = [];
	let visibleEntries: string[] = [];
	let skillDirs: string[] = [];
	let exposureView: string | null = null;
	/** `name → entry id` of the skills this session's view links (what `skill://<name>` opens). */
	let viewSkills = new Map<string, string>();
	let hooks: Hook[] = [];
	let callHooks: Hook[] = [];
	let contextHooks: Hook[] = [];
	let retrieveHooks: Hook[] = [];
	let tracksCalls = false;
	/** `match.instructions.exposed`: personal AGENTS.md sections omp loads in this session. */
	let instructionsExposed: Json[] = [];
	/** `match` saw `REMATCH_ROOT/profile/.git`: session start runs one `profile sync` (DESIGN §20). */
	let profilePresent = false;
	let profileSynced = false;

	/** Mount `dirs` as this process's rematch skill dirs and withhold `withheld` agents (null: all of rematch's). Never throws. */
	async function applyScope(dirs: readonly string[], withheld: readonly string[] | null): Promise<void> {
		try {
			const configured = await overrideList(pi, "skills.customDirectories", persisted => sessionSkillDirs(persisted, dirs, root));
			skillDirs = [...dirs];
			for (const name of shadowedByCustomDirs(viewSkills, configured, root)) viewSkills.delete(name);
		} catch (error) {
			log("warn", `skill dirs not set: ${errorText(error)}`);
		}
		try {
			const names = withheld ?? rematchAgentNames(root);
			await overrideList(pi, "task.disabledAgents", configured => sessionDisabledAgents(configured, names));
		} catch (error) {
			log("warn", `agent scope not set: ${errorText(error)}`);
		}
	}

	/** Match and sync `cwd`, adopt its entries, hooks and skill dirs; `fresh` drops every cached result first. Never throws. */
	async function loadMatch(cwd: string, fresh: boolean): Promise<void> {
		if (!fresh && cwd === loadedCwd) return;
		loadedCwd = cwd;
		if (!enabled || !bin) {
			await applyScope([], null);
			return;
		}
		if (fresh) matchCache.clear();
		const key = `${bin}\0${cwd}`;
		let pending = matchCache.get(key);
		if (!pending) {
			pending = Promise.all([
				call(["match", "--cwd", cwd], { timeout: 5000 }),
				call(["skills", "sync", "--cwd", cwd, "--harness", "omp"], { timeout: 5000 }),
			]).then(([match, sync]) => ({ match, sync }));
			matchCache.set(key, pending);
		}
		const { match, sync } = await pending;
		if (!match.ok || !sync.ok) matchCache.delete(key);
		try {
			if (!match.ok) throw new Error(match.error ?? "match failed");
			const data = match.data ?? {};
			if (data.off === true) {
				enabled = false;
				await applyScope([], null);
				return;
			}
			profilePresent = data.profile_present === true;
			const next: Hook[] = [];
			for (const file of stringsOf(data.hooks)) {
				try {
					next.push(await loadHook(file));
				} catch (error) {
					log("warn", `hook ${file} skipped: ${errorText(error)}`);
				}
			}
			loadedEntries = loadedEntriesOf(data, next);
			hooks = next;
			callHooks = next.filter(h => h.mode === "block" || h.mode === "stuck");
			contextHooks = next.filter(h => h.mode === "truncate" || h.mode === "retrieve");
			retrieveHooks = next.filter(h => h.mode === "retrieve");
			tracksCalls = next.some(h => h.mode === "stuck");
			instructionsExposed = recordsOf(isRecord(data.instructions) ? data.instructions.exposed : undefined);
		} catch (error) {
			log("warn", `match skipped for ${cwd}: ${errorText(error)}`);
		}
		try {
			if (!sync.ok) throw new Error(sync.error ?? "skills sync failed");
			visibleEntries = stringsOf(sync.data?.visible);
			exposureView = typeof sync.data?.view === "string" ? sync.data.view : null;
			viewSkills = new Map();
			const links = isRecord(sync.data?.links) ? sync.data.links : {};
			for (const [name, dir] of Object.entries(links)) {
				const id = typeof dir === "string" ? entryOfSkillDir(dir) : null;
				if (id) viewSkills.set(name, id);
			}
			await applyScope(stringsOf(sync.data?.dirs), withheldAgentsOf(sync));
		} catch (error) {
			log("warn", `skills skipped for ${cwd}: ${errorText(error)}`);
			await applyScope([], null);
		}
	}

	let loading: Promise<void> = Promise.resolve();
	/** Serialized `loadMatch`: a context event never sees a half-adopted state. */
	function reload(cwd: string, fresh: boolean): Promise<void> {
		loading = loading.then(() => loadMatch(cwd, fresh));
		return loading;
	}

	if (enabled && !bin) log("warn", "binary not found; tracing without staging");
	await reload(process.cwd(), false);

	// --- per-session trace state (closure: one binding = one session) ----------------------------
	let trace: SessionTrace | null = null;
	let loadedAnnotated = false;
	const calls = new CallHistory();
	/** `tool\0hook ids` whose evaluation this session already traced (`hook_evaluated`, once each). */
	const evaluated = new Set<string>();
	/** The last traced truncation (`hook_applied`): written again only when what was cut changes. */
	let lastTruncation = "";
	let lastRetrieval: { key: string; units: RetrievedUnit[] } | null = null;
	let lastTracedUnits = "";
	/** The appended retrieval message, reused while the unit contents are unchanged. */
	let lastInjected: { content: string; message: Json } | null = null;

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
			// On Windows the binary must detach its own worker from the host's job object;
			// unref only releases the JavaScript handle, not the OS process lifetime.
			const args = process.platform === "win32" ? ["reflect", "--pending", "--detach"] : ["reflect", "--pending"];
			Bun.spawn({ cmd: [bin, ...args], stdio: ["ignore", "ignore", "ignore"] }).unref();
		} catch (error) {
			log("warn", `reflect spawn failed: ${errorText(error)}`);
		}
	}

	/** Toplevel and HEAD in one call; an unborn HEAD still yields the toplevel (exit 1). */
	async function gitTop(cwd: string): Promise<{ top: string | null; head: string | null }> {
		try {
			const r = await pi.exec("git", ["rev-parse", "--show-toplevel", "--verify", "-q", "HEAD"], { cwd, timeout: 3000 });
			const [top, head] = r.stdout.trim().split("\n").map(s => s.trim());
			if (r.killed || (r.code !== 0 && r.code !== 1) || !top) return { top: null, head: null };
			return { top, head: r.code === 0 && /^[0-9a-f]{40,64}$/.test(head ?? "") ? head : null };
		} catch {
			return { top: null, head: null };
		}
	}

	/** The origin URL as git prints it (the binary normalizes it); null without origin, on timeout or error. */
	async function gitRemote(cwd: string): Promise<string | null> {
		try {
			const r = await pi.exec("git", ["remote", "get-url", "origin"], { cwd, timeout: 3000 });
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
			file: path.join(root, "traces", `${idFileStem(id)}.jsonl`),
			seq: 0,
			lastInput: null,
			lastDeliverable: null,
			agentRunning: false,
			errors: new ErrorWindow(),
			reasons: new Set(),
			turnsSinceAgentEnd: 0,
			lastModel: modelOf(ctx),
			dirReady: false,
			cwd: ctx.cwd,
			repo: null,
			changed: new Set(),
		};
		const t = trace;
		const { top: repo, head } = await gitTop(ctx.cwd);
		const remote = repo ? await gitRemote(ctx.cwd) : null;
		t.repo = repo;
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
			staging_dirs: skillDirs,
			repo_remote: remote,
			subpath: repo ? subpathOf(repo, ctx.cwd) : null,
			head_sha: head,
			visible_entries: visibleEntries,
			exposure_view: exposureView,
			instructions_exposed: instructionsExposed.map(x => ({ id: typeof x.id === "string" ? x.id : null, name: x.name, sha: x.sha })),
		});
		if (!loadedAnnotated) {
			loadedAnnotated = true;
			try {
				pi.appendEntry("com.vncp.rematch.loaded", { entries: loadedEntries.map(e => e.id), hooks: hooks.map(h => h.id) });
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
		const files = filesChanged([...t.changed], t.cwd, t.repo, Date.now() + FILES_CHANGED_BUDGET_MS);
		write("session_shutdown", files && files.length > 0 ? { reasons, flagged, files_changed: files } : { reasons, flagged });
		if (flagged) {
			try {
				const dir = path.join(root, "pending");
				fs.mkdirSync(dir, { recursive: true });
				fs.writeFileSync(
					path.join(dir, `${idFileStem(t.id)}.json`),
					JSON.stringify({ session_id: t.id, flagged_at: Date.now(), reasons, trace: t.file }),
				);
			} catch (error) {
				log("warn", `pending flag write failed: ${errorText(error)}`);
			}
		}
		trace = null;
	}

	/** News as toasts, and the actions waiting as a persistent footer status (never a dialog: the session is not blocked). */
	function presentDigest(ctx: ExtensionContext, env: Envelope): ReviewAction[] {
		const lines = stringsOf(env.data?.lines);
		for (const line of lines.slice(0, 5)) ctx.ui.notify(`rematch: ${line}`, "info");
		if (lines.length > 5) ctx.ui.notify(`rematch: +${lines.length - 5} more (/rematch digest)`, "info");
		const actions = reviewActionsOf(env.data);
		try {
			ctx.ui.setStatus("rematch", pendingStatus(actions));
		} catch (error) {
			log("debug", `setStatus failed: ${errorText(error)}`);
		}
		if (actions.length > 0) ctx.ui.notify(`rematch: ${actions.length} action(s) waiting for you — /rematch review (alt+e); nothing changes until you decide`, "info");
		return actions;
	}

	async function showDigest(ctx: ExtensionContext, quietWhenEmpty: boolean): Promise<ReviewAction[]> {
		if (!ctx.hasUI) return [];
		const env = await call(["digest", "--cwd", ctx.cwd], { timeout: 10_000 });
		if (!env.ok) {
			if (!quietWhenEmpty) ctx.ui.notify(`rematch: digest failed: ${env.error}`, "warning");
			return [];
		}
		const empty = stringsOf(env.data?.lines).length === 0 && recordsOf(env.data?.actions).length === 0;
		if (empty && !quietWhenEmpty) ctx.ui.notify("rematch: nothing new", "info");
		return presentDigest(ctx, env);
	}

	/**
	 * One dialog per waiting action: Approve / Decline run exactly the command the binary offers
	 * (with the hash shown); Inspect shows the whole text in the dialog (never in the conversation)
	 * and asks again; Later or Esc leaves it pending. Blocked and conflict actions are shown with
	 * their reason and commands, no dialog.
	 */
	async function review(ctx: ExtensionContext, actions: readonly ReviewAction[]): Promise<void> {
		if (!ctx.hasUI) return;
		let changed = false;
		for (const a of actions) {
			if (!(a.approve && !a.blocked) && !a.decline) {
				ctx.ui.notify(`rematch: ${a.kind} ${a.name}: ${a.summary}${a.blocked ? ` — ${a.blocked}` : ""} (rematch library inspect ${a.id})`, "warning");
				continue;
			}
			let title = `rematch: ${a.kind} ${a.name} — ${a.summary}`;
			let choice: string | undefined;
			for (;;) {
				choice = await ctx.ui.select(title, reviewChoices(a));
				if (choice !== REVIEW_INSPECT) break;
				const inspected = await call(["library", "inspect", a.id]);
				const text = inspected.ok ? stringsOf(inspected.data?.lines).join("\n") : (inspected.error ?? "");
				title = `rematch: ${a.kind} ${a.name}\n\n${text}`;
			}
			const argv = reviewArgv(a, choice);
			if (!argv) continue;
			const r = await call(argv);
			if (r.ok) changed = true;
			const verb = choice === REVIEW_APPROVE ? "approved" : "declined";
			ctx.ui.notify(`rematch: ${verb} ${a.name}${r.ok ? (a.reload ? ` (${a.reload})` : "") : ` (failed: ${r.error})`}`, r.ok ? "info" : "warning");
		}
		// Hook modules and agent links change now; skills and personal text load from the next session.
		if (changed) {
			await reload(ctx.cwd, true);
			await showDigest(ctx, true);
		}
	}

	async function startSession(ctx: ExtensionContext, withDigest: boolean): Promise<void> {
		calls.clear();
		evaluated.clear();
		lastTruncation = "";
		lastRetrieval = null;
		lastTracedUnits = "";
		lastInjected = null;
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

	// --- tool + command (registered even when off, so `/rematch on` works) --------------------------
	function recordMark(cls: string | null) {
		if (!cls || !enabled || !trace) return;
		write("mark", { class: cls });
		trace.reasons.add(`mark:${cls}`);
	}

	async function toggle(on: boolean, ctx: ExtensionContext | undefined): Promise<Envelope> {
		const operation = on ? "on" : "off";
		if (on && hardOff) return { ok: false, operation, error: "REMATCH=0 is set in the environment; unset it and restart omp" };
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
				env.data = { ...env.data, note: "tracing resumes now; skills apply from the next omp start" };
				await reload(ctx?.cwd ?? process.cwd(), true);
				if (!trace && ctx && isMain(ctx)) await startSession(ctx, false);
			} else {
				// Withdraw now what this process still offers: rematch agents and skill dirs (the skill
				// list and any text already sent stay in this session; a new session starts clean).
				await applyScope([], null);
				env.data = { ...env.data, note: "rematch agents withdrawn now; skills and personal instructions already in this session stay until you start a new one" };
			}
		}
		return env;
	}

	async function runOperation(operation: string, args: readonly string[], ctx: ExtensionContext | undefined, signal?: AbortSignal) {
		const sid = trace?.id ?? (ctx ? sessionIdOf(ctx) : null);
		const argv = buildArgv(operation, args, sid, ctx?.cwd ?? process.cwd());
		if (operation === "off" || operation === "on") return toggle(operation === "on", ctx);
		if (operation === "mark") recordMark(markClass(argv));
		const env = await call(argv, { signal, timeout: timeoutFor(operation) });
		if (env.ok && changesLoaded(argv)) await reload(ctx?.cwd ?? loadedCwd ?? process.cwd(), true);
		return env;
	}

	const z = pi.zod;
	pi.registerTool({
		name: "rematch",
		label: "rematch",
		description:
			"rematch control plane: traces sessions, stages provisional skills/scripts/hooks grounded in traces, keeps or retires them by counters. " +
			"operation = binary subcommand (doctor, status, library, why, digest, match, create, improve, activate, run, transfer, ship, mark, off, on, …); " +
			'arguments = the remaining argv as separate strings, e.g. {"operation":"library","arguments":["ls"]}. Long work returns a job id; poll with status. ' +
			"Approving, declining, pinning, disabling or rolling back an entry, approving an export, marking a turn, switching rematch off or on, and uninstalling are reserved to the user (/rematch or the session-start digest).",
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
				// The argv the binary would run, whatever order the global flags come in.
				const refused = userOnlyReason(buildArgv(params.operation, args, "-", ctx?.cwd ?? process.cwd()));
				if (refused) throw new Error(refused);
				value = await runOperation(params.operation, args, ctx, signal);
			} catch (error) {
				value = { ok: false, operation: params.operation, error: errorText(error) };
			}
			return { content: [{ type: "text", text: JSON.stringify(value) }], details: value, isError: value.ok !== true };
		},
	});

	pi.registerCommand("rematch", {
		description: `rematch: /rematch ${COMMANDS.join("|")} [args]`,
		handler: async (raw, ctx) => {
			let value: Envelope;
			try {
				const words = shellWords(raw ?? "");
				if (words[0] === "review" && ctx.hasUI && enabled) {
					// The dialog review: Approve / Decline / Inspect / Later per action, nothing on Esc.
					await review(ctx, await showDigest(ctx, true));
					return;
				}
				const target = reviewTarget(words);
				if (target && ctx.hasUI) {
					// A typed approve/decline without a hash: this action's dialog, which carries it.
					const inspected = await call(["library", "inspect", target]);
					if (!inspected.ok) throw new Error(inspected.error ?? `no action ${target}`);
					await review(ctx, reviewActionsOf({ actions: [inspected.data?.action] }));
					return;
				}
				const { operation, args } = commandToOperation(words);
				if (!enabled && !["on", "status", "doctor", "library", "why"].includes(operation)) {
					throw new Error(hardOff ? "rematch is off (REMATCH=0)" : "rematch is off; /rematch on to enable");
				}
				let finalArgs = args;
				if (operation === "uninstall" && !args.includes("--yes")) {
					const ok = ctx.hasUI && (await ctx.ui.confirm("rematch: uninstall?", "Removes every file rematch wrote, nothing else."));
					if (!ok) throw new Error("uninstall not confirmed (run /rematch uninstall --yes)");
					finalArgs = [...args, "--yes"];
				}
				value = await runOperation(operation, finalArgs, ctx);
			} catch (error) {
				value = { ok: false, error: errorText(error) };
			}
			const lines = stringsOf(value.ok ? value.data?.lines : undefined);
			const content = lines.length > 0 ? lines.join("\n") : JSON.stringify(value, null, 2);
			try {
				pi.sendMessage({ customType: "rematch", content, display: true, details: value });
			} catch (error) {
				log("warn", `sendMessage failed: ${errorText(error)}`);
			}
		},
	});

	if (hardOff) return;

	// Registering a shortcut also makes the TUI load this extension eagerly (DESIGN §0).
	pi.registerShortcut("alt+e", {
		description: "rematch: digest and review what waits for you",
		handler: async ctx => {
			try {
				await review(ctx, await showDigest(ctx, false));
			} catch (error) {
				log("warn", `shortcut failed: ${errorText(error)}`);
			}
		},
	});

	// --- tool_call: the user's controls, then hooks (block, stuck): fail closed -------------------
	pi.on("tool_call", (event, ctx) => {
		// On or off: `rematch on` from the model's shell would be the switch the user did not flip.
		const refused = event.toolName === "bash" && isRecord(event.input) ? userOnlyShellReason(event.input.command) : null;
		if (refused) {
			if (isMain(ctx)) write("tool_blocked", { tool: event.toolName, call_id: event.toolCallId, hook_id: "rematch:user-only", reason: refused, args_hash: argsHash(event.input) });
			return { block: true, reason: refused };
		}
		if (!enabled || callHooks.length === 0) return undefined;
		let key: string | null = null;
		let blocked: { hook: Hook; reason: string } | null = null;
		for (const hook of callHooks) {
			let reason: string | null;
			try {
				if (hook.mode === "block") reason = blockReason(hook, event.toolName, event.input);
				else {
					key ??= callKey(event.toolName, event.input);
					reason = stuckReason(hook, event.toolName, calls.run(key));
				}
			} catch (error) {
				reason = `rematch hook ${hook.id} failed: ${errorText(error)}`;
			}
			if (reason) {
				blocked = { hook, reason };
				break;
			}
		}
		// A call a `block` rule rejected never ran, so it is no repetition; a call `stuck` rejected stays
		// in the run, so the next identical call is rejected too.
		if (tracksCalls && (!blocked || blocked.hook.mode === "stuck")) {
			try {
				calls.push(key ?? callKey(event.toolName, event.input));
			} catch {
				// an unhashable input has nothing to repeat
			}
		}
		if (!blocked) {
			// The hooks evaluated this call and let it pass: that is use of them, not only loading.
			const evaluatedBy = callHooks.filter(h => toolMatches(h, event.toolName)).map(h => h.id);
			const sig = `${event.toolName}\0${evaluatedBy.join("\0")}`;
			if (evaluatedBy.length > 0 && trace && isMain(ctx) && !evaluated.has(sig)) {
				evaluated.add(sig);
				write("hook_evaluated", { hook_ids: evaluatedBy, tool: event.toolName });
			}
			return undefined;
		}
		if (isMain(ctx)) write("tool_blocked", { tool: event.toolName, call_id: event.toolCallId, hook_id: blocked.hook.id, reason: blocked.reason });
		return { block: true, reason: blocked.reason };
	});

	/** `retrieve` (main sessions only, so the trace can attribute it): one message or null. */
	async function retrieveMessage(messages: readonly unknown[], ctx: ExtensionContext): Promise<Json | null> {
		const hook = retrieveHooks[0];
		if (!hook || !trace || !isMain(ctx)) return null;
		const query = retrievalQuery(messages);
		if (!query) return null;
		// The binary bounds candidates to the entries loaded for --cwd (`retrieve::candidates`).
		const argv = [
			"retrieve", "--cwd", ctx.cwd, "--budget", String(hook.params.budget_bytes), "--min-overlap", String(hook.params.min_overlap),
			"--query", query,
		];
		const key = argv.join("\0");
		let units: RetrievedUnit[];
		if (lastRetrieval?.key === key) units = lastRetrieval.units;
		else {
			const env = await call(argv, { timeout: 3000 });
			if (!env.ok) return null;
			units = retrievedUnits(env.data);
			lastRetrieval = { key, units };
		}
		if (units.length === 0) return null;
		const signature = units.map(u => `${u.entry_id}:${u.path}`).join("|");
		if (signature !== lastTracedUnits) {
			lastTracedUnits = signature;
			const traced = units.map(u => ({ entry_id: u.entry_id, path: u.path, bytes: Buffer.byteLength(u.text, "utf8") }));
			write("retrieval", { entry_ids: retrieveHooks.map(h => h.id), units: traced, bytes: traced.reduce((n, u) => n + u.bytes, 0) });
		}
		// One message object per unit content: a cache hit or an identical answer re-sends it unchanged.
		const content = JSON.stringify(units);
		if (lastInjected?.content !== content) lastInjected = { content, message: retrievalMessage(units) };
		return lastInjected.message;
	}

	// --- context hooks (truncate, retrieve): fail open, outgoing request only ---------------------
	pi.on("context", async (event, ctx) => {
		if (!enabled) return undefined;
		// A main session that moved to another cwd loads that cwd's entries and hooks first.
		if (isMain(ctx) && ctx.cwd !== loadedCwd) await reload(ctx.cwd, false);
		if (contextHooks.length === 0) return undefined;
		let messages: unknown[] | null = null;
		try {
			messages = truncateToolResults(event.messages, contextHooks);
			const cut = messages ? messages.filter((m, i) => m !== event.messages[i]).length : 0;
			const sig = `${cut}`;
			if (messages && trace && isMain(ctx) && sig !== lastTruncation) {
				lastTruncation = sig;
				for (const h of contextHooks.filter(h => h.mode === "truncate")) {
					write("hook_applied", { hook_id: h.id, mode: "truncate", detail: `${cut} tool result(s) cut in the outgoing request` });
				}
			}
		} catch (error) {
			log("warn", `truncate hook failed: ${errorText(error)}`);
		}
		try {
			const extra = await retrieveMessage(messages ?? event.messages, ctx);
			if (extra) messages = [...(messages ?? event.messages), extra];
		} catch (error) {
			log("warn", `retrieve hook failed: ${errorText(error)}`);
		}
		return messages ? { messages: messages as typeof event.messages } : undefined;
	});

	// --- trace events: fail open -----------------------------------------------------------------
	pi.on(
		"session_start",
		guard("session_start", async (_event, ctx) => {
			if (!enabled || !isMain(ctx)) return;
			await reload(ctx.cwd, false);
			await startSession(ctx, true);
			// The profile (a git repo) is pulled, relinked and pushed once per process, in the
			// background: bounded by the binary (`--timeout 8`, offline fails open) and by the exec.
			if (profilePresent && !profileSynced) {
				profileSynced = true;
				void call(["profile", "sync", "--timeout", "8"], { timeout: 10_000 }).then(r => {
					if (r.ok && r.data?.offline === true) log("debug", `profile sync offline: ${String(r.data?.error ?? "")}`);
				});
			}
		}),
	);

	const rotate = guard("session_rotate", async (_event: unknown, ctx) => {
		if (!enabled || !isMain(ctx)) return;
		if (trace && trace.id === sessionIdOf(ctx)) return;
		closeTrace();
		// Another session: what loads is matched afresh (the library may have changed since).
		await reload(ctx.cwd, true);
		await startSession(ctx, false);
	});
	pi.on("session_switch", rotate);
	pi.on("session_branch", rotate);

	pi.on(
		"input",
		guard("input", (event: { text: string; source: string }, ctx) => {
			if (!enabled || !trace || !isMain(ctx)) return;
			const text = typeof event.text === "string" ? event.text : "";
			// A takeover is an input while the agent loop is running. `ctx.isIdle()` is also false
			// while a finished turn settles, which made ordinary prompts count as takeovers (harm).
			const idle = !trace.agentRunning;
			const label = prelabel(text, idle, trace.lastInput);
			// Slash commands and `!`/`%` local executions are not user turns; only `/rematch mark`
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
				pi.appendEntry("com.vncp.rematch.redirect", { seq, prelabel: label, text_head: head(text, 200) });
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
				skill_entry: event.toolName === "read" && !isError ? skillEntryOfRead(rawPath, viewSkills, ctx.cwd) : null,
				path: typeof rawPath === "string" ? stripSelectors(rawPath) : null,
			});
			if (trace.errors.push(isError)) trace.reasons.add("tool_error_cluster");
			if (!isError) for (const p of changedPaths(event.toolName, input)) trace.changed.add(p);
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
		"agent_start",
		guard("agent_start", (_event: unknown, ctx) => {
			if (!trace || !isMain(ctx)) return;
			trace.agentRunning = true;
		}),
	);

	pi.on(
		"agent_end",
		guard("agent_end", (event: { messages: unknown; willContinue?: boolean }, ctx) => {
			if (!trace || !isMain(ctx)) return;
			trace.agentRunning = event.willContinue === true;
			if (!enabled) return;
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
