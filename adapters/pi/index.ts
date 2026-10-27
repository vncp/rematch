// rematch for Pi (`@earendil-works/pi-coding-agent` ≥ 1.4, DESIGN §18.3; API facts in API-NOTES.md).
// Traces main sessions into <root>/traces/pi:<id>[:r<n>].jsonl (DESIGN §3), contributes the shared
// skills dir through `resources_discover`, runs approved hooks (`block`/`stuck` on `tool_call`,
// `truncate`/`retrieve` on `context`), and exposes the `rematch` tool and the `/rematch` command.
// Every decision beyond the cheap prelabels lives in the binary; the pure helpers are the omp
// extension's (`extension/index.ts`), shared, not copied. Loaded by jiti under Node or Bun: only
// `node:` APIs, no `Bun` global.
import { type ChildProcess, spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type { ExecResult, ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { getAgentDir, VERSION } from "@earendil-works/pi-coding-agent";
import {
	appendPromptFragments,
	argsHash,
	buildArgv,
	CallHistory,
	callKey,
	changedPaths,
	changesLoaded,
	COMMANDS,
	commandToOperation,
	contentBytes,
	decode,
	type Envelope,
	ErrorWindow,
	errorText,
	FILES_CHANGED_BUDGET_MS,
	filesChanged,
	entryOfSkillDir,
	head,
	type Hook,
	hookFromModule,
	idFileStem,
	isExecutable,
	isRecord,
	LIMITS,
	type LoadedEntry,
	markClass,
	OPERATIONS,
	type Operation,
	pendingStatus,
	prelabel,
	RETRIEVAL_TYPE,
	type RetrievedUnit,
	retrievalQuery,
	retrievedUnits,
	type PromptFragment,
	promptFragmentsOf,
	renderPromptFragments,
	recordsOf,
	REVIEW_APPROVE,
	REVIEW_INSPECT,
	type ReviewAction,
	reviewActionsOf,
	reviewArgv,
	reviewChoices,
	reviewTarget,
	userOnlyReason,
	userOnlyShellReason,
	rematchRoot,
	resolveBinary,
	shellWords,
	skillEntryOfRead,
	skillFromReadPath,
	skillsInvoked,
	stringsOf,
	stripSelectors,
	stuckReason,
	subpathOf,
	textOf,
	timeoutFor,
	truncateToolInput,
	truncateToolResults,
	blockReason,
} from "../../extension/index.ts";

type Json = Record<string, unknown>;
type ModelRef = { provider: string; id: string };

// ---------------------------------------------------------------------------------------------
// Identity (DESIGN §18.3): `pi:<id>`, a resumed finished trace continues as `pi:<id>:r<n>`.

export const ID_PREFIX = "pi:";

/** `pi:<id>` with every character outside `[A-Za-z0-9._-]` replaced by `_` (rematch-core `import_pi::session_id`). */
export function traceId(sessionId: string): string {
	return `${ID_PREFIX}${sessionId.replace(/[^A-Za-z0-9._-]/g, "_")}`;
}

export function segmentId(base: string, n: number): string {
	return n <= 1 ? base : `${base}:r${n}`;
}

/** Highest existing segment of `base` in `dir`: `{n, id}` (n = 0 when none exists). */
export function latestSegment(dir: string, base: string): { n: number; id: string } {
	let n = 0;
	while (fs.existsSync(path.join(dir, `${idFileStem(segmentId(base, n + 1))}.jsonl`))) n++;
	return { n, id: segmentId(base, Math.max(n, 1)) };
}

/** A trace file's records (unparseable lines skipped). */
export function readTrace(file: string): Json[] {
	let text: string;
	try {
		text = fs.readFileSync(file, "utf8");
	} catch {
		return [];
	}
	const out: Json[] = [];
	for (const line of text.split("\n")) {
		if (!line.trim()) continue;
		try {
			const v: unknown = JSON.parse(line);
			if (isRecord(v)) out.push(v);
		} catch {
			// a torn last line: skipped
		}
	}
	return out;
}

/** A `session_shutdown` after the last `session_start` (the binary's `reflect::finished`). */
export function finished(records: readonly Json[]): boolean {
	let lastStart = -1;
	records.forEach((r, i) => {
		if (r.ev === "session_start") lastStart = i;
	});
	return records.slice(Math.max(lastStart, 0)).some(r => r.ev === "session_shutdown");
}

/** What a trace continues from: the next seq, prelabel context, flag state, call history. */
export type Restored = {
	seq: number;
	lastInput: string | null;
	lastDeliverable: string | null;
	lastModel: ModelRef | null;
	lastStop: string | null;
	reasons: Set<string>;
	errors: ErrorWindow;
	changed: Set<string>;
	callKeys: string[];
	turnsSinceAgentEnd: number;
};

function modelOfRecord(v: unknown): ModelRef | null {
	return isRecord(v) && typeof v.provider === "string" && typeof v.id === "string" ? { provider: v.provider, id: v.id } : null;
}

/**
 * Replays `records` (one trace file) into the adapter's in-memory state. Flag reasons follow
 * rematch-core `trace::flag_reasons`; the call history keeps `tool_result`s and the rejections
 * of `stuck` hooks (`stuckIds`), as the live handler does.
 */
export function restoreState(records: readonly Json[], stuckIds: ReadonlySet<string> = new Set()): Restored {
	const st: Restored = {
		seq: 0,
		lastInput: null,
		lastDeliverable: null,
		lastModel: null,
		lastStop: null,
		reasons: new Set(),
		errors: new ErrorWindow(),
		changed: new Set(),
		callKeys: [],
		turnsSinceAgentEnd: 0,
	};
	for (const r of records) {
		if (typeof r.seq === "number") st.seq = Math.max(st.seq, r.seq + 1);
		switch (r.ev) {
			case "session_start":
				st.lastModel = modelOfRecord(r.model) ?? st.lastModel;
				break;
			case "input": {
				if (typeof r.text === "string") st.lastInput = r.text;
				const label = typeof r.prelabel === "string" ? r.prelabel : "none";
				if (label && label !== "none") st.reasons.add(label.startsWith("mark:") ? label : `redirect:${label}`);
				break;
			}
			case "mark":
				if (typeof r.class === "string") st.reasons.add(`mark:${r.class}`);
				break;
			case "tool_result": {
				if (st.errors.push(r.is_error === true)) st.reasons.add("tool_error_cluster");
				if (typeof r.tool === "string" && typeof r.args_hash === "string") st.callKeys.push(`${r.tool}\0${r.args_hash}`);
				if (r.is_error !== true && typeof r.tool === "string") for (const p of changedPaths(r.tool, r.input)) st.changed.add(p);
				break;
			}
			case "tool_blocked":
				if (typeof r.hook_id === "string" && stuckIds.has(r.hook_id) && typeof r.tool === "string" && typeof r.args_hash === "string") {
					st.callKeys.push(`${r.tool}\0${r.args_hash}`);
				}
				break;
			case "turn_end":
				st.lastModel = modelOfRecord(r.model) ?? st.lastModel;
				if (typeof r.text_head === "string" && r.text_head.trim()) st.lastDeliverable = r.text_head;
				st.lastStop = typeof r.stop_reason === "string" ? r.stop_reason : null;
				st.turnsSinceAgentEnd++;
				break;
			case "model_change":
				st.lastModel = modelOfRecord(r.to) ?? st.lastModel;
				break;
			case "agent_end":
				st.turnsSinceAgentEnd = 0;
				break;
		}
	}
	st.callKeys = st.callKeys.slice(-32);
	return st;
}

/** Loaded entry ids (`id@version`) and hook ids none of the trace's `session_start`s recorded. */
export function unrecordedLoad(records: readonly Json[], loaded: readonly LoadedEntry[], hookIds: readonly string[]): boolean {
	const seen = new Set<string>();
	for (const r of records) {
		if (r.ev !== "session_start") continue;
		for (const e of Array.isArray(r.loaded_entries) ? r.loaded_entries : []) {
			if (typeof e === "string") seen.add(`${e}@`);
			else if (isRecord(e) && typeof e.id === "string") seen.add(`${e.id}@${typeof e.version === "number" ? e.version : ""}`);
		}
		for (const h of stringsOf(r.hooks_loaded)) seen.add(`hook:${h}`);
	}
	return loaded.some(e => !seen.has(`${e.id}@${e.version ?? ""}`)) || hookIds.some(h => !seen.has(`hook:${h}`));
}

/**
 * How a session start opens its trace (DESIGN §18.3): no file → a new trace `pi:<id>`; the latest
 * segment unfinished → continue it (a `/reload` writes a `session_start` only when what loads
 * changed — an approval, activation or rollback — so that load is credited; anything else writes
 * one: a resumed crash leaves one trace that `count` folds once, whole); the latest segment
 * finished → a new segment `pi:<id>:r<n+1>`, so a session counted before it was resumed is never
 * finalized again and its resumed events still count.
 */
export function planTrace(dir: string, base: string, reason: string): { id: string; continues: boolean; writeStart: boolean; previous: string | null } {
	const latest = latestSegment(dir, base);
	if (latest.n === 0) return { id: base, continues: false, writeStart: true, previous: null };
	const file = path.join(dir, `${idFileStem(latest.id)}.jsonl`);
	if (!finished(readTrace(file))) return { id: latest.id, continues: true, writeStart: reason !== "reload", previous: file };
	return { id: segmentId(base, latest.n + 1), continues: false, writeStart: true, previous: file };
}

// ---------------------------------------------------------------------------------------------
// Hook modules: parsed as data (rematch `trace_event::parse_hook_module`), never imported.

/** The `export default { key: <json>, … };` lines of a generated hook module → its object. */
export function parseHookModule(text: string): Json {
	const at = text.indexOf("export default {");
	if (at < 0) throw new Error("no default export");
	const out: Json = {};
	for (const raw of text.slice(at + "export default {".length).split("\n")) {
		const line = raw.trim();
		if (line.startsWith("}")) break;
		const sep = line.indexOf(": ");
		if (sep < 0) continue;
		const key = line.slice(0, sep).trim();
		const value = line.slice(sep + 2).replace(/,$/, "");
		out[key] = JSON.parse(value);
	}
	return out;
}

export function loadHookFile(file: string): Hook {
	return hookFromModule(parseHookModule(fs.readFileSync(file, "utf8")), path.basename(file).replace(/\.[cm]?[jt]s$/, ""));
}

// ---------------------------------------------------------------------------------------------
// User authority: approvals, off and marks are the user's (DESIGN §18.3).

/** Tool operations the model may not run: the user's decisions, judged on the argv the binary would run (global flags anywhere). Null when allowed. */
export function refusedForModel(operation: string, args: readonly string[]): string | null {
	return userOnlyReason(buildArgv(operation, args, "-", process.cwd()));
}

// ---------------------------------------------------------------------------------------------
// Process boundary. `pi.exec` takes no env: the binary is spawned here so Pi's
// `PI_CODING_AGENT_DIR` (also omp's name) never redirects rematch's omp paths or the omp editor's
// roles; Pi's own dirs go as `REMATCH_PI_AGENT_DIR` / `REMATCH_PI_SESSION_DIR`.

export function binaryEnv(env: NodeJS.ProcessEnv, piAgentDir: string): NodeJS.ProcessEnv {
	const out: NodeJS.ProcessEnv = { ...env, REMATCH_PI_AGENT_DIR: piAgentDir };
	if (env.PI_CODING_AGENT_SESSION_DIR) out.REMATCH_PI_SESSION_DIR = env.PI_CODING_AGENT_SESSION_DIR;
	delete out.PI_CODING_AGENT_DIR;
	delete out.PI_CODING_AGENT_SESSION_DIR;
	return out;
}

export function run(bin: string, argv: readonly string[], opts: { timeout: number; signal?: AbortSignal; cwd?: string; env: NodeJS.ProcessEnv }): Promise<ExecResult> {
	const { promise, resolve } = Promise.withResolvers<ExecResult>();
	let stdout = "";
	let stderr = "";
	let killed = false;
	let child: ChildProcess;
	try {
		child = spawn(bin, [...argv], { cwd: opts.cwd, env: opts.env, stdio: ["ignore", "pipe", "pipe"] });
	} catch (error) {
		resolve({ stdout: "", stderr: errorText(error), code: 127, killed: false });
		return promise;
	}
	const kill = () => {
		killed = true;
		child.kill("SIGTERM");
	};
	const timer = setTimeout(kill, opts.timeout);
	timer.unref?.();
	if (opts.signal?.aborted) kill();
	opts.signal?.addEventListener("abort", kill, { once: true });
	child.stdout?.on("data", d => (stdout += d));
	child.stderr?.on("data", d => (stderr += d));
	const done = (code: number) => {
		clearTimeout(timer);
		opts.signal?.removeEventListener("abort", kill);
		resolve({ stdout, stderr, code, killed });
	};
	child.on("error", error => {
		stderr += errorText(error);
		done(127);
	});
	child.on("close", code => done(code ?? 1));
	return promise;
}

/** `$OMP_BIN`, else `omp` on PATH: reflection's editor and classifier are `omp -p` calls (DESIGN §9). */
export function ompAvailable(env: NodeJS.ProcessEnv = process.env): boolean {
	if (env.OMP_BIN) return isExecutable(env.OMP_BIN);
	return (env.PATH ?? "").split(path.delimiter).some(d => d && isExecutable(path.join(d, "omp")));
}

// ---------------------------------------------------------------------------------------------
// Messages

export const OUTPUT_TYPE = "rematch";

/**
 * The one message `retrieve` appends: a Pi `CustomMessage`, which Pi's `convertToLlm` sends as a
 * user-role message (Pi has no developer role for extension context, API-NOTES §context).
 */
export function retrievalMessage(units: readonly RetrievedUnit[], timestamp = Date.now()): Json {
	const body = units.map(u => `<unit skill="${u.name}" path="${u.path}">\n${u.text.trimEnd()}\n</unit>`).join("\n\n");
	return {
		role: "custom",
		customType: RETRIEVAL_TYPE,
		display: false,
		timestamp,
		content: `[rematch] Units of skills loaded in this session that match the current request, retrieved by the harness (the whole skill: its SKILL.md in the skills list).\n\n${body}`,
		details: { units: units.map(u => ({ entry_id: u.entry_id, path: u.path })) },
	};
}

/**
 * `session_start.loaded_entries` of a `match` result under Pi: predicate hits, minus `retrieve`
 * hooks (credited through `retrieval`), agents (Pi has no subagent definitions) and hook entries
 * whose module did not load.
 */
export function piLoadedEntries(data: unknown, hooks: readonly Hook[]): LoadedEntry[] {
	const retrieveIds = new Set(hooks.filter(h => h.mode === "retrieve").map(h => h.id));
	const running = new Set(hooks.map(h => h.id));
	return (isRecord(data) ? recordsOf(data.entries) : [])
		.filter(e => typeof e.id === "string" && e.predicate_hit !== false && !retrieveIds.has(e.id))
		.filter(e => e.kind !== "agent" && e.kind !== "lesson" && (e.kind !== "hook" || running.has(e.id as string)))
		.map(e => {
			const out: LoadedEntry = { id: e.id as string };
			if (typeof e.version === "number") out.version = e.version;
			if (typeof e.sha === "string" && e.sha) out.sha = e.sha;
			return out;
		});
}

// ---------------------------------------------------------------------------------------------
// Factory

type SessionTrace = Restored & {
	id: string;
	file: string;
	cwd: string;
	repo: string | null;
	/** Between `agent_start` and `agent_settled`. */
	running: boolean;
};

const PROFILE_SYNCED = Symbol.for("com.vncp.rematch.pi.profileSynced");

export default async function rematch(pi: ExtensionAPI): Promise<void> {
	const root = rematchRoot();
	const tracesDir = path.join(root, "traces");
	const hardOff = process.env.REMATCH === "0";
	let enabled = !hardOff && !fs.existsSync(path.join(root, "off"));
	const bin = resolveBinary();
	let piAgentDir: string;
	try {
		piAgentDir = getAgentDir();
	} catch {
		piAgentDir = path.join(process.env.HOME ?? "", ".pi", "agent");
	}
	const env = binaryEnv(process.env, piAgentDir);

	const log = (msg: string) => {
		if (process.env.REMATCH_DEBUG) process.stderr.write(`rematch: ${msg}\n`);
	};

	async function call(argv: string[], opts: { timeout?: number; signal?: AbortSignal; cwd?: string } = {}): Promise<Envelope> {
		const operation = argv[0] ?? "";
		if (!bin) return { ok: false, operation, error: "rematch binary not found ($REMATCH_BIN, target/{release,debug}, PATH)" };
		const result = await run(bin, argv, { timeout: opts.timeout ?? timeoutFor(operation), signal: opts.signal, cwd: opts.cwd, env });
		const value = decode(result, operation);
		if (!value.ok) log(`${operation} failed: ${value.error} ${head(result.stderr, 600)}`);
		return value;
	}

	// --- what loads: entries, hooks, the shared skills dir --------------------------------------
	let loadedEntries: LoadedEntry[] = [];
	let visibleEntries: string[] = [];
	/** Approved personal prompt fragments of the entries loaded for this cwd. */
	let promptFragments: PromptFragment[] = [];
	/** The fragment set this trace already recorded as sent (`fragments_sent`). */
	let sentFragments = "";
	let skillDirs: string[] = [];
	let exposureView: string | null = null;
	/** `name → entry id` of the skills this session's view links. */
	let viewSkills = new Map<string, string>();
	let hooks: Hook[] = [];
	let callHooks: Hook[] = [];
	let contextHooks: Hook[] = [];
	let retrieveHooks: Hook[] = [];
	let profilePresent = false;

	/** Match and sync `cwd`; a failed match keeps the previous hooks, a failed sync adds no dir. Never throws. */
	async function loadMatch(cwd: string): Promise<void> {
		if (!enabled || !bin) return;
		const [match, sync] = await Promise.all([
			call(["match", "--cwd", cwd], { timeout: 5000 }),
			call(["skills", "sync", "--cwd", cwd, "--harness", "pi"], { timeout: 5000 }),
		]);
		if (match.ok) {
			const data = match.data ?? {};
			if (data.off === true) {
				enabled = false;
				return;
			}
			profilePresent = data.profile_present === true;
			const next: Hook[] = [];
			for (const file of stringsOf(data.hooks)) {
				try {
					next.push(loadHookFile(file));
				} catch (error) {
					log(`hook ${file} skipped: ${errorText(error)}`);
				}
			}
			loadedEntries = piLoadedEntries(data, next);
			hooks = next;
			callHooks = next.filter(h => h.mode === "block" || h.mode === "stuck");
			contextHooks = next.filter(h => h.mode === "truncate" || h.mode === "retrieve");
			retrieveHooks = next.filter(h => h.mode === "retrieve");
			promptFragments = promptFragmentsOf(data);
		}
		if (sync.ok && sync.data?.off !== true) {
			visibleEntries = stringsOf(sync.data?.visible);
			// This place's view (kept library + this place's provisional entries), per session.
			skillDirs = stringsOf(sync.data?.dirs);
			exposureView = typeof sync.data?.view === "string" ? sync.data.view : null;
			viewSkills = new Map();
			const links = isRecord(sync.data?.links) ? sync.data.links : {};
			for (const [name, dir] of Object.entries(links)) {
				const id = typeof dir === "string" ? entryOfSkillDir(dir) : null;
				if (id) viewSkills.set(name, id);
			}
		} else {
			// Off, or no answer: nothing of rematch's is offered (never another place's view).
			skillDirs = [];
			viewSkills = new Map();
		}
	}

	let loading: Promise<void> = Promise.resolve();
	function reloadMatch(cwd: string): Promise<void> {
		loading = loading.then(() => loadMatch(cwd));
		return loading;
	}

	// --- trace ------------------------------------------------------------------------------------
	let trace: SessionTrace | null = null;
	const calls = new CallHistory();
	/** Calls a rematch hook rejected (traced as `tool_blocked`, never as `tool_result`). */
	const blockedCalls = new Set<string>();
	/** Arguments of calls in flight, from `tool_execution_start`. */
	const callArgs = new Map<string, unknown>();
	let lastRetrieval: { key: string; units: RetrievedUnit[] } | null = null;
	let lastTracedUnits = "";
	let lastInjected: { content: string; message: Json } | null = null;

	const modelOf = (ctx: ExtensionContext): ModelRef | null => (ctx.model ? { provider: ctx.model.provider, id: ctx.model.id } : null);

	function write(ev: string, fields: Json): number | null {
		const t = trace;
		if (!t) return null;
		try {
			fs.mkdirSync(tracesDir, { recursive: true });
			const seq = t.seq;
			fs.appendFileSync(t.file, `${JSON.stringify({ seq, t: Date.now(), ev, ...fields })}\n`);
			t.seq++;
			return seq;
		} catch (error) {
			log(`trace write failed: ${errorText(error)}`);
			return null;
		}
	}

	async function gitTop(cwd: string): Promise<{ top: string | null; head: string | null }> {
		try {
			const r = await pi.exec("git", ["rev-parse", "--show-toplevel", "--verify", "-q", "HEAD"], { cwd, timeout: 3000 });
			const [top, h] = r.stdout.trim().split("\n").map(s => s.trim());
			if (r.killed || (r.code !== 0 && r.code !== 1) || !top) return { top: null, head: null };
			return { top, head: r.code === 0 && /^[0-9a-f]{40,64}$/.test(h ?? "") ? h : null };
		} catch {
			return { top: null, head: null };
		}
	}

	async function gitRemote(cwd: string): Promise<string | null> {
		try {
			const r = await pi.exec("git", ["remote", "get-url", "origin"], { cwd, timeout: 3000 });
			const out = r.stdout.trim();
			return r.code === 0 && !r.killed && out ? out : null;
		} catch {
			return null;
		}
	}

	async function openTrace(ctx: ExtensionContext, reason: string): Promise<void> {
		let raw: string;
		try {
			raw = ctx.sessionManager.getSessionId();
		} catch {
			raw = "";
		}
		if (!raw) return;
		const plan = planTrace(tracesDir, traceId(raw), reason);
		const stuckIds = new Set(hooks.filter(h => h.mode === "stuck").map(h => h.id));
		const prior = plan.previous ? restoreState(readTrace(plan.previous), stuckIds) : restoreState([]);
		// A new segment starts its own counts; it keeps only the context for prelabels.
		const base = plan.continues
			? prior
			: { ...restoreState([]), lastInput: prior.lastInput, lastDeliverable: prior.lastDeliverable, lastModel: prior.lastModel };
		sentFragments = "";
		trace = {
			...base,
			lastModel: base.lastModel ?? modelOf(ctx),
			id: plan.id,
			file: path.join(tracesDir, `${idFileStem(plan.id)}.jsonl`),
			cwd: ctx.cwd,
			repo: null,
			running: false,
		};
		calls.clear();
		if (plan.continues) for (const k of base.callKeys) calls.push(k);
		if (plan.writeStart) {
			await writeStart(ctx, `pi:${reason}`);
			return;
		}
		const records = readTrace(trace.file);
		const start = records.find(r => r.ev === "session_start");
		trace.repo = typeof start?.repo === "string" ? start.repo : null;
		// A reload re-bound after an approval, activation or rollback: what loads now is recorded.
		if (unrecordedLoad(records, loadedEntries, hooks.map(h => h.id))) await writeStart(ctx, `pi:${reason}`);
	}

	/**
	 * `session_start` with what loads now. A second one in a trace adds to the session's loaded
	 * set (`fold_trace` unions them) and keeps the trace unfinished until its `session_shutdown`.
	 */
	async function writeStart(ctx: ExtensionContext, mode: string): Promise<void> {
		const t = trace;
		if (!t) return;
		const { top: repo, head: headSha } = await gitTop(ctx.cwd);
		t.repo = repo;
		const remote = repo ? await gitRemote(ctx.cwd) : null;
		write("session_start", {
			session_id: t.id,
			cwd: ctx.cwd,
			repo,
			branch_hint: path.basename(repo ?? ctx.cwd),
			model: modelOf(ctx),
			mode,
			agent: { kind: "main", name: "main" },
			omp_version: `pi ${VERSION}`,
			loaded_entries: loadedEntries,
			hooks_loaded: hooks.map(h => h.id),
			staging_dirs: skillDirs,
			repo_remote: remote,
			subpath: repo ? subpathOf(repo, ctx.cwd) : null,
			head_sha: headSha,
			visible_entries: visibleEntries,
			exposure_view: exposureView,
		});
	}

	/** After an in-place re-match (digest approval, tool activation): record a changed loaded set. */
	async function creditLoaded(ctx: ExtensionContext, mode: string): Promise<void> {
		if (trace && unrecordedLoad(readTrace(trace.file), loadedEntries, hooks.map(h => h.id))) await writeStart(ctx, mode);
	}

	/** Close the open run, write `session_shutdown`; flagged ⇒ `pending/<id>.json`. */
	function closeTrace(): void {
		const t = trace;
		if (!t) return;
		if (t.running || t.turnsSinceAgentEnd > 0) {
			write("agent_end", { turns: t.turnsSinceAgentEnd, is_terminal: true, yielded: false });
			t.turnsSinceAgentEnd = 0;
		}
		const reasons = [...t.reasons];
		const flagged = reasons.length > 0;
		const files = filesChanged([...t.changed], t.cwd, t.repo, Date.now() + FILES_CHANGED_BUDGET_MS);
		write("session_shutdown", files && files.length > 0 ? { reasons, flagged, files_changed: files } : { reasons, flagged });
		if (flagged) {
			try {
				const dir = path.join(root, "pending");
				fs.mkdirSync(dir, { recursive: true });
				fs.writeFileSync(path.join(dir, `${idFileStem(t.id)}.json`), JSON.stringify({ session_id: t.id, flagged_at: Date.now(), reasons, trace: t.file, source: "pi" }));
			} catch (error) {
				log(`pending flag write failed: ${errorText(error)}`);
			}
		}
		trace = null;
	}

	function spawnReflect(): void {
		if (!bin || !ompAvailable()) return;
		try {
			const args = process.platform === "win32" ? ["reflect", "--pending", "--detach"] : ["reflect", "--pending"];
			const child = spawn(bin, args, { detached: true, stdio: "ignore", env });
			child.on("error", error => log(`reflect spawn failed: ${errorText(error)}`));
			child.unref();
		} catch (error) {
			log(`reflect spawn failed: ${errorText(error)}`);
		}
	}

	// --- digest (UI only: never in the provider context) -----------------------------------------
	async function showDigest(ctx: ExtensionContext): Promise<ReviewAction[]> {
		if (!ctx.hasUI) return [];
		const env = await call(["digest", "--cwd", ctx.cwd], { timeout: 10_000 });
		if (!env.ok) return [];
		const lines = stringsOf(env.data?.lines);
		for (const line of lines.slice(0, 5)) ctx.ui.notify(`rematch: ${line}`, "info");
		if (lines.length > 5) ctx.ui.notify(`rematch: +${lines.length - 5} more (/rematch digest)`, "info");
		const actions = reviewActionsOf(env.data);
		ctx.ui.setStatus("rematch", pendingStatus(actions));
		if (actions.length > 0) ctx.ui.notify(`rematch: ${actions.length} action(s) waiting for you — /rematch review; nothing changes until you decide`, "info");
		return actions;
	}

	/** Approve / Decline / Inspect / Later per action (Esc = Later); blocked and conflict actions are shown, not asked. `true` when something changed. */
	async function review(ctx: ExtensionContext, actions: readonly ReviewAction[]): Promise<boolean> {
		let changed = false;
		for (const a of actions) {
			if (!(a.approve && !a.blocked) && !a.decline) {
				ctx.ui.notify(`rematch: ${a.kind} ${a.name}: ${a.summary}${a.blocked ? ` — ${a.blocked}` : ""} (rematch library inspect ${a.id})`, "warning");
				continue;
			}
			const choices = reviewChoices(a);
			const legend = choices.map(c => `${c.label}: ${c.description ?? ""}`).join("\n");
			let title = `rematch: ${a.kind} ${a.name} — ${a.summary}\n${legend}`;
			let choice: string | undefined;
			for (;;) {
				choice = await ctx.ui.select(title, choices.map(c => c.label));
				if (choice !== REVIEW_INSPECT) break;
				const inspected = await call(["library", "inspect", a.id]);
				title = `rematch: ${a.kind} ${a.name}\n\n${inspected.ok ? stringsOf(inspected.data?.lines).join("\n") : (inspected.error ?? "")}\n\n${legend}`;
			}
			const argv = reviewArgv(a, choice);
			if (!argv) continue;
			const r = await call(argv);
			if (r.ok) changed = true;
			ctx.ui.notify(`rematch: ${choice === REVIEW_APPROVE ? "approved" : "declined"} ${a.name}${r.ok ? (a.reload ? ` (${a.reload})` : "") : ` (failed: ${r.error})`}`, r.ok ? "info" : "warning");
		}
		// An approved hook runs from the next call; its skill links reach the list at the next /reload.
		if (changed) {
			await reloadMatch(ctx.cwd);
			await creditLoaded(ctx, "pi:approve");
		}
		return changed;
	}

	// --- tool + command (registered even when off, so `/rematch on` works) ------------------------
	function recordMark(cls: string | null): void {
		if (!cls || !enabled || !trace) return;
		write("mark", { class: cls });
		trace.reasons.add(`mark:${cls}`);
	}

	async function toggle(on: boolean): Promise<Envelope> {
		const operation = on ? "on" : "off";
		if (on && hardOff) return { ok: false, operation, error: "REMATCH=0 is set in the environment; unset it and restart pi" };
		let env = await call([operation]);
		if (!env.ok && !bin) {
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
		if (env.ok && !on) {
			// Off ends this segment now: what ran so far stays countable, nothing more is traced.
			closeTrace();
			enabled = false;
		}
		if (env.ok && on) enabled = true;
		return env;
	}

	async function runOperation(operation: string, args: readonly string[], ctx: ExtensionContext | undefined, signal?: AbortSignal): Promise<Envelope> {
		if (operation === "off" || operation === "on") return toggle(operation === "on");
		const argv = buildArgv(operation, args, trace?.id ?? null, ctx?.cwd ?? process.cwd());
		if (operation === "mark") recordMark(markClass(argv));
		return call(argv, { signal, timeout: timeoutFor(operation) });
	}

	pi.registerTool({
		name: "rematch",
		label: "rematch",
		description:
			"rematch control plane: traces sessions, stages provisional skills/scripts/hooks grounded in traces, keeps or retires them by counters. " +
			"operation = binary subcommand (doctor, status, library, why, digest, match, create, improve, activate, run, transfer, ship, …); " +
			'arguments = the remaining argv as separate strings, e.g. {"operation":"library","arguments":["ls"]}. Long work returns a job id; poll with status. ' +
			"Approving, declining, disabling, rolling back, marking, turning rematch off/on and uninstalling are reserved to the user (/rematch).",
		parameters: {
			type: "object",
			properties: {
				operation: { type: "string", enum: [...OPERATIONS] },
				arguments: { type: "array", items: { type: "string" } },
			},
			required: ["operation"],
			additionalProperties: false,
		} as never,
		async execute(_id, rawParams, signal, _onUpdate, ctx) {
			const params = rawParams as { operation: Operation; arguments?: string[] };
			let value: Envelope;
			try {
				const args = params.arguments ?? [];
				const refused = refusedForModel(params.operation, args);
				if (refused) throw new Error(refused);
				if (!enabled && !["status", "doctor", "library", "why"].includes(params.operation)) throw new Error("rematch is off");
				value = await runOperation(params.operation, args, ctx, signal);
				// Re-match in this runtime (hooks apply from the next call); the skill list refreshes at /reload.
				if (value.ok && changesLoaded(buildArgv(params.operation, args, null, ctx.cwd))) {
					await reloadMatch(ctx.cwd);
					await creditLoaded(ctx, `pi:${params.operation}`);
				}
			} catch (error) {
				value = { ok: false, operation: params.operation, error: errorText(error) };
			}
			return { content: [{ type: "text", text: JSON.stringify(value) }], details: value, isError: value.ok !== true };
		},
	});

	function report(ctx: ExtensionContext, value: Envelope): void {
		const lines = stringsOf(value.ok ? value.data?.lines : undefined);
		const content = lines.length > 0 ? lines.join("\n") : JSON.stringify(value, null, 2);
		if (ctx.hasUI) {
			// Shown in the transcript (deferred to the end of a streaming turn, never steering it); the
			// `context` handler keeps it out of every provider request.
			pi.sendMessage({ customType: OUTPUT_TYPE, content, display: true, details: value }, { triggerTurn: false });
		} else {
			process.stderr.write(`${content}\n`);
		}
	}

	pi.registerCommand("rematch", {
		description: `rematch: /rematch ${COMMANDS.join("|")} [args]`,
		handler: async (raw: string, ctx: ExtensionCommandContext) => {
			let value: Envelope | null;
			let reloadAfter = false;
			try {
				const { operation, args } = commandToOperation(shellWords(raw ?? ""));
				if (!enabled && !["on", "status", "doctor", "library", "why"].includes(operation)) {
					throw new Error(hardOff ? "rematch is off (REMATCH=0)" : "rematch is off; /rematch on to enable");
				}
				let finalArgs = args;
				if (operation === "uninstall" && !args.includes("--yes")) {
					const ok = ctx.hasUI && (await ctx.ui.confirm("rematch: uninstall?", "Removes every file rematch wrote, nothing else."));
					if (!ok) throw new Error("uninstall not confirmed (run /rematch uninstall --yes)");
					finalArgs = [...args, "--yes"];
				}
				const target = reviewTarget(shellWords(raw ?? ""));
				if (target && ctx.hasUI) {
					// A typed approve/decline without a hash: this action's dialog, which carries it.
					const inspected = await call(["library", "inspect", target]);
					if (!inspected.ok) throw new Error(inspected.error ?? `no action ${target}`);
					reloadAfter = await review(ctx, reviewActionsOf({ actions: [inspected.data?.action] }));
					value = null;
				} else if ((operation === "digest" || shellWords(raw ?? "")[0] === "review") && ctx.hasUI) {
					// Lines as notifications; actions on `/rematch review` as Approve/Decline/Inspect/Later dialogs.
					const actions = await showDigest(ctx);
					reloadAfter = shellWords(raw ?? "")[0] === "review" ? await review(ctx, actions) : false;
					value = null;
				} else {
					value = await runOperation(operation, finalArgs, ctx, ctx.signal);
					reloadAfter =
						value.ok &&
						(operation === "off" || operation === "on" || changesLoaded(buildArgv(operation, finalArgs, null, ctx.cwd)));
				}
			} catch (error) {
				value = { ok: false, error: errorText(error) };
			}
			if (value) report(ctx, value);
			// Hooks, skills and handlers are re-bound from scratch (DESIGN §18.3): an approval, a
			// withdrawal or off/on takes effect in this session, not the next one.
			if (reloadAfter) await ctx.reload();
		},
	});

	if (hardOff) return;

	// --- skills: the shared dir after `session_start` synced it ----------------------------------
	pi.on("resources_discover", () => {
		if (!enabled || skillDirs.length === 0) return undefined;
		return { skillPaths: skillDirs };
	});

	// --- tool_call hooks (block, stuck): fail closed ---------------------------------------------
	pi.on("tool_call", event => {
		// On or off: `rematch on` from the model's shell would be the switch the user did not flip.
		const refused = event.toolName === "bash" && isRecord(event.input) ? userOnlyShellReason(event.input.command) : null;
		if (refused) {
			write("tool_blocked", { tool: event.toolName, call_id: event.toolCallId, hook_id: "rematch:user-only", reason: refused, args_hash: argsHash(event.input) });
			blockedCalls.add(event.toolCallId);
			return { block: true, reason: refused };
		}
		if (!enabled) return undefined;
		if (callHooks.length === 0) return undefined;
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
		if (callHooks.some(h => h.mode === "stuck") && (!blocked || blocked.hook.mode === "stuck")) {
			try {
				calls.push(key ?? callKey(event.toolName, event.input));
			} catch {
				// an unhashable input has nothing to repeat
			}
		}
		if (!blocked) return undefined;
		write("tool_blocked", { tool: event.toolName, call_id: event.toolCallId, hook_id: blocked.hook.id, reason: blocked.reason, args_hash: argsHash(event.input) });
		blockedCalls.add(event.toolCallId);
		return { block: true, reason: blocked.reason };
	});

	async function retrieveMessage(messages: readonly unknown[], ctx: ExtensionContext): Promise<Json | null> {
		const hook = retrieveHooks[0];
		if (!hook || !trace) return null;
		const query = retrievalQuery(messages);
		if (!query) return null;
		const argv = ["retrieve", "--cwd", ctx.cwd, "--budget", String(hook.params.budget_bytes), "--min-overlap", String(hook.params.min_overlap), "--query", query];
		const key = argv.join("\0");
		let units: RetrievedUnit[];
		if (lastRetrieval?.key === key) units = lastRetrieval.units;
		else {
			const env = await call(argv, { timeout: 3000, signal: ctx.signal });
			if (!env.ok) return null;
			units = retrievedUnits(env.data);
			lastRetrieval = { key, units };
		}
		if (units.length === 0) return null;
		const signature = units.map(u => `${u.entry_id}:${u.path}`).join("|");
		if (signature !== lastTracedUnits) {
			lastTracedUnits = signature;
			const traced = units.map(u => ({ entry_id: u.entry_id, path: u.path, bytes: Buffer.byteLength(u.text, "utf8") }));
			// Only the hook whose parameters ran is credited.
			write("retrieval", { entry_ids: [hook.id], units: traced, bytes: traced.reduce((n, u) => n + u.bytes, 0) });
		}
		const content = JSON.stringify(units);
		if (lastInjected?.content !== content) lastInjected = { content, message: retrievalMessage(units) };
		return lastInjected.message;
	}

	// --- context: our transcript output out, then truncate + retrieve (fail open) ------------------
	pi.on("context", async (event, ctx) => {
		let messages: unknown[] | null = null;
		const own = event.messages.filter(m => !(isRecord(m) && m.role === "custom" && m.customType === OUTPUT_TYPE));
		if (own.length !== event.messages.length) messages = own;
		if (enabled && contextHooks.length > 0) {
			try {
				messages = truncateToolResults(messages ?? event.messages, contextHooks) ?? messages;
			} catch (error) {
				log(`truncate hook failed: ${errorText(error)}`);
			}
			try {
				const extra = await retrieveMessage(messages ?? event.messages, ctx);
				if (extra) messages = [...(messages ?? event.messages), extra];
			} catch (error) {
				log(`retrieve hook failed: ${errorText(error)}`);
			}
		}
		return messages ? { messages: messages as typeof event.messages } : undefined;
	});

	// --- personal prompt fragments: appended to the system prompt, fail open ---------------------
	// Pi's `systemPrompt` is one string (`{ systemPrompt?: string }` is the result type). A handler may
	// re-enter, so this one returns the base prompt with the block appended, every time, and keeps no
	// state.
	pi.on("before_agent_start", event => {
		try {
			if (!enabled) return undefined;
			const systemPrompt = appendPromptFragments((event as { systemPrompt?: unknown }).systemPrompt, promptFragments);
			if (typeof systemPrompt !== "string") return undefined;
			// Record the set this prompt actually carries (approved sha each), once per set per trace.
			const sig = promptFragments.map(f => `${f.id}:${f.sha ?? ""}`).join("|");
			if (trace && sig !== sentFragments) {
				sentFragments = sig;
				const bytes = Buffer.byteLength(renderPromptFragments(promptFragments) ?? "", "utf8");
				write("fragments_sent", { host: "pi", fragments: promptFragments.map(f => ({ id: f.id, name: f.name, sha: f.sha ?? "", via: "pi:before_agent_start" })), bytes });
			}
			return { systemPrompt };
		} catch (error) {
			log(`prompt fragments failed: ${errorText(error)}`);
			return undefined;
		}
	});

	/** Event handlers fail open: tracing must never disturb the session. */
	function guard<E>(name: string, fn: (event: E, ctx: ExtensionContext) => unknown) {
		return async (event: E, ctx: ExtensionContext): Promise<void> => {
			try {
				await fn(event, ctx);
			} catch (error) {
				log(`${name} handler failed: ${errorText(error)}`);
			}
		};
	}

	pi.on(
		"session_start",
		guard("session_start", async (event: { reason: string }, ctx) => {
			if (!enabled) return;
			await reloadMatch(ctx.cwd);
			if (!enabled) return;
			lastRetrieval = null;
			lastTracedUnits = "";
			lastInjected = null;
			await openTrace(ctx, event.reason);
			if (event.reason === "reload") return;
			spawnReflect();
			// Dialogs may outlive the handler: presented detached, never throwing.
			void showDigest(ctx).catch(error => log(`digest failed: ${errorText(error)}`));
			const g = globalThis as Record<symbol, unknown>;
			if (profilePresent && !g[PROFILE_SYNCED]) {
				g[PROFILE_SYNCED] = true;
				void call(["profile", "sync", "--timeout", "8"], { timeout: 10_000 });
			}
		}),
	);

	pi.on(
		"session_shutdown",
		guard("session_shutdown", (event: { reason: string }) => {
			// A reload re-binds this runtime to the same session: its trace continues.
			if (event.reason === "reload") {
				trace = null;
				return;
			}
			if (enabled) closeTrace();
		}),
	);

	pi.on(
		"input",
		guard("input", (event: { text: string; source: string; streamingBehavior?: string }) => {
			if (!enabled || !trace) return;
			// `pi.sendUserMessage` from an extension: not typed by the human.
			if (event.source === "extension") return;
			const text = typeof event.text === "string" ? event.text : "";
			// A takeover is input steered into the running loop. A follow-up typed meanwhile waits for
			// the run to end and a prompt after an Esc abort starts a new one: both idle (the abort is
			// the run's `agent_end` with `yielded: false`).
			const idle = event.streamingBehavior !== "steer";
			const label = prelabel(text, idle, trace.lastInput);
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
				log(`appendEntry redirect failed: ${errorText(error)}`);
			}
		}),
	);

	// Every finished call, from `tool_execution_end`: Pi's `tool_result` hook runs only for calls
	// that executed, not for unknown tools, invalid arguments or calls another extension blocked,
	// which the model still sees as failed results. The arguments come from `tool_execution_start`.
	// Calls made by another tool (codemode) are not in the transcript: the call that made them is.
	pi.on(
		"tool_execution_start",
		guard("tool_execution_start", (event: { toolCallId: string; args: unknown; parentToolCallId?: string }) => {
			if (enabled && trace && !event.parentToolCallId) callArgs.set(event.toolCallId, event.args ?? null);
		}),
	);

	pi.on(
		"tool_execution_end",
		guard("tool_execution_end", (event: { toolName: string; toolCallId: string; parentToolCallId?: string; result: unknown; isError: boolean }, ctx) => {
			if (!enabled || !trace || event.parentToolCallId) return;
			const input = callArgs.has(event.toolCallId) ? callArgs.get(event.toolCallId) : null;
			callArgs.delete(event.toolCallId);
			// A call a rematch hook rejected never ran: its `tool_blocked` record stands for it.
			if (blockedCalls.delete(event.toolCallId)) return;
			const content = isRecord(event.result) ? event.result.content : undefined;
			const rawPath = isRecord(input) ? (input.path ?? input.file_path) : undefined;
			const isError = event.isError === true;
			write("tool_result", {
				tool: event.toolName,
				call_id: event.toolCallId,
				input: truncateToolInput(input),
				args_hash: argsHash(input),
				result_bytes: contentBytes(content),
				is_error: isError,
				error_class: isError ? "error" : null,
				content_head: head(textOf(content), LIMITS.contentHead),
				skill_read: event.toolName === "read" && !isError ? skillFromReadPath(rawPath) : null,
				skill_entry: event.toolName === "read" && !isError ? skillEntryOfRead(rawPath, viewSkills, ctx.cwd) : null,
				path: typeof rawPath === "string" ? stripSelectors(rawPath) : null,
			});
			if (trace.errors.push(isError)) trace.reasons.add("tool_error_cluster");
			if (!isError) for (const p of changedPaths(event.toolName, input)) trace.changed.add(p);
		}),
	);

	pi.on(
		"turn_end",
		guard("turn_end", (event: { message: unknown }) => {
			if (!enabled || !trace) return;
			const msg = isRecord(event.message) ? event.message : null;
			if (!msg || msg.role !== "assistant") return;
			const model = typeof msg.provider === "string" && typeof msg.model === "string" ? { provider: msg.provider, id: msg.model } : null;
			const last = trace.lastModel;
			if (model && last && (model.provider !== last.provider || model.id !== last.id)) write("model_change", { from: last, to: model, reason: "turn" });
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
			trace.lastStop = typeof msg.stopReason === "string" ? msg.stopReason : null;
			trace.turnsSinceAgentEnd++;
		}),
	);

	pi.on(
		"model_select",
		guard("model_select", (event: { model: { provider: string; id: string }; previousModel?: { provider: string; id: string }; source: string }) => {
			if (!enabled || !trace || event.source === "restore") return;
			const to = { provider: event.model.provider, id: event.model.id };
			const from = event.previousModel ? { provider: event.previousModel.provider, id: event.previousModel.id } : trace.lastModel;
			if (from && from.provider === to.provider && from.id === to.id) return;
			write("model_change", { from, to, reason: `select:${event.source}` });
			trace.lastModel = to;
		}),
	);

	pi.on(
		"agent_start",
		guard("agent_start", () => {
			if (trace) trace.running = true;
		}),
	);

	// `agent_end` is not final in Pi (retries, compaction, queued work start another run); the run
	// the user sees ends at `agent_settled`.
	pi.on(
		"agent_settled",
		guard("agent_settled", (event: { aborted: boolean }) => {
			if (!enabled || !trace) return;
			trace.running = false;
			write("agent_end", { turns: trace.turnsSinceAgentEnd, is_terminal: true, yielded: !event.aborted && trace.lastStop === "stop" });
			trace.turnsSinceAgentEnd = 0;
		}),
	);
}
