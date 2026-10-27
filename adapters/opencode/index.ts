// rematch for OpenCode: a server plugin (OpenCode 1.19.x, `@opencode-ai/plugin` Hooks) that
// forwards OpenCode's own hook and event payloads to the `rematch` binary, which owns every
// mapping to the trace (DESIGN §3, §18; `rematch trace-event --harness opencode`). The plugin
// keeps only transport state: per-session ordered queues, the text of streaming assistant
// messages, which payloads it already sent, and the approved hooks matched for its directory.
//
// Hooks used: `config` (shared skills dir into `skills.paths`, the `/rematch-*` commands),
// `event` (session.created/updated, message.part.updated, message.updated, session.idle),
// `chat.message`, `command.execute.before`, `tool.execute.before` (approved block/stuck rules
// via `rematch hook-check`, and the guard that keeps approvals the user's), `tool` (the
// read-only `rematch` tool), `experimental.chat.messages.transform` (approved `truncate`
// rules, outgoing request only), `experimental.chat.system.transform` (approved `retrieve`
// units, per request, not stored), `dispose`. Every failure of the bridge is logged and
// swallowed: only an approved, intentional block throws.
import { type ChildProcess, spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type { Hooks, Plugin, PluginInput } from "@opencode-ai/plugin";
import { decode, type Envelope, errorText, isRecord, rematchRoot, resolveBinary, truncationMarker } from "../../extension/index.ts";

type Json = Record<string, unknown>;
type SessionInfo = { parent: string | null; directory: string | null; version: string | null };
type TruncateRule = { tool: string; max_age_turns: number; keep_chars: number };

/** Subcommands that run no TUI: no digest toast there (it would be consumed unseen). */
const HEADLESS: Record<string, true> = Object.fromEntries(
	["run", "serve", "web", "acp", "attach", "github", "pr", "mcp", "models", "auth", "stats", "export", "import", "debug", "upgrade", "uninstall", "agent", "session", "db", "plugin", "generate", "completion"].map(
		c => [c, true],
	),
);

/** Read-only operations the model may run through the `rematch` tool. */
const TOOL_OPERATIONS: Record<string, string[]> = {
	status: ["status"],
	doctor: ["doctor"],
	"library-ls": ["library", "ls"],
	"library-why": ["library", "why"],
	match: ["match"],
	"builtins-ls": ["builtins", "ls"],
};

/** `rematch <args>` that only the user runs: library approve/decline/pin/unpin/disable/rollback, export --approve, on, off, uninstall. */
const USER_ONLY_LIBRARY: Record<string, true> = { approve: true, decline: true, pin: true, unpin: true, disable: true, rollback: true };

function userOnlyArgs(args: readonly string[]): boolean {
	const positional: string[] = [];
	let approveFlag = false;
	for (let i = 0; i < args.length; i++) {
		const a = args[i] as string;
		// Global flags (`--text`, `--cwd DIR`, `--cwd=DIR`) may stand anywhere, clap accepts them after the subcommand too.
		if (a === "--cwd") {
			i++;
			continue;
		}
		if (a.startsWith("-")) {
			if (a === "--approve" || a === "--approve-all") approveFlag = true;
			continue;
		}
		positional.push(a);
	}
	const [cmd, sub] = positional;
	if (cmd === "on" || cmd === "off" || cmd === "uninstall") return true;
	return cmd === "library" && ((sub !== undefined && USER_ONLY_LIBRARY[sub] === true) || (sub === "export" && approveFlag));
}

/** Words that may stand before the command itself: runners (`sudo rematch …`, `sh rematch.sh …`) and their flags. */
const COMMAND_PREFIX: Record<string, true> = { env: true, exec: true, command: true, sudo: true, nohup: true, time: true, sh: true, bash: true, zsh: true };

/**
 * Whether a shell line runs one of the user's controls (`/rematch-approve`, `/rematch-decline`,
 * `/rematch-on`, `/rematch-off` are the user's commands): a `rematch` / `rematch.sh` word,
 * quoted or by path, in command position (after env assignments and runners such as `sudo`,
 * `sh`, or the quoted script of `sh -c`) of any `;`/`&&`/`|`/subshell segment, followed by
 * those words with global flags anywhere. Mentions elsewhere (`git commit -m "… rematch off"`,
 * `echo rematch on`) pass. A guard against the direct command, not a sandbox: an arbitrary
 * script, `eval` of a built string or an alias still runs.
 */
export function isApprovalCommand(command: unknown): boolean {
	if (typeof command !== "string") return false;
	for (const segment of command.split(/&&|\|\||[;&|\n()`]|\$\(/)) {
		const words = (segment.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g) ?? []).map(w => w.replace(/["']/g, ""));
		for (let i = 0; i < words.length; i++) {
			const word = words[i] as string;
			const base = word.split("/").pop() ?? "";
			if (base === "rematch" || base === "rematch.sh") {
				if (userOnlyArgs(words.slice(i + 1))) return true;
				break;
			}
			// The quoted script of `sh -c "…"` is a command line of its own.
			if (/\s/.test(word) && i > 0) {
				if (isApprovalCommand(word)) return true;
				break;
			}
			if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(word) || word.startsWith("-") || COMMAND_PREFIX[base] === true) continue;
			break;
		}
	}
	return false;
}

/**
 * What a `retrieve` reply lets the plugin cache for the session until its next prompt or tool
 * result: the units' text, or `null` for "no units for this input". `undefined` (ask again on
 * the next request) for a failed or timed-out call, rematch off, or a call made before the
 * session's prompt reached the trace.
 */
export function retrievalCacheValue(reply: Json | undefined): string | null | undefined {
	if (!reply || typeof reply.error === "string" || reply.off === true || reply.has_input !== true) return undefined;
	return typeof reply.context === "string" ? reply.context : null;
}

/** OpenCode's `experimental.chat.messages.transform` messages. */
type OcMessage = { info: Json; parts: Json[] };

function isTypedUser(m: OcMessage): boolean {
	return (
		m.info.role === "user" &&
		m.parts.some(p => p.type === "text" && p.synthetic !== true && p.ignored !== true && typeof p.text === "string" && p.text.trim() !== "")
	);
}

/**
 * `truncate` (DESIGN §10) on OpenCode's message shape: walking back from the newest message,
 * age = typed user messages after a tool result; a completed tool part older than a rule's
 * `max_age_turns` keeps the first `keep_chars` of its output (the strictest rule wins) plus
 * the marker. Parts are replaced, never edited: the stored session keeps the full output.
 * Returns the number of outputs shortened.
 */
export function truncateToolOutputs(messages: OcMessage[], rules: readonly TruncateRule[]): number {
	if (rules.length === 0) return 0;
	let changed = 0;
	let age = 0;
	for (let i = messages.length - 1; i >= 0; i--) {
		const m = messages[i];
		if (!m || !isRecord(m.info) || !Array.isArray(m.parts)) continue;
		if (isTypedUser(m)) {
			age++;
			continue;
		}
		if (m.info.role !== "assistant") continue;
		const parts = m.parts.map(p => {
			if (p.type !== "tool" || !isRecord(p.state) || p.state.status !== "completed" || typeof p.state.output !== "string") return p;
			let keep = Number.POSITIVE_INFINITY;
			for (const r of rules) {
				if ((r.tool === "*" || r.tool === p.tool) && age > r.max_age_turns) keep = Math.min(keep, r.keep_chars);
			}
			const text = p.state.output;
			if (!Number.isFinite(keep) || text.length <= keep) return p;
			const last = text.charCodeAt(keep - 1);
			const cut = last >= 0xd800 && last <= 0xdbff ? keep - 1 : keep;
			changed++;
			return { ...p, state: { ...p.state, output: `${text.slice(0, cut)}\n${truncationMarker(text.length - cut, age)}` } };
		});
		if (parts.some((p, j) => p !== m.parts[j])) messages[i] = { ...m, parts };
	}
	return changed;
}

function which(name: string): string | null {
	for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
		if (!dir) continue;
		const p = path.join(dir, name);
		try {
			fs.accessSync(p, fs.constants.X_OK);
			if (fs.statSync(p).isFile()) return p;
		} catch {
			// not here
		}
	}
	return null;
}

/** `'…'` for a POSIX shell. */
function shq(s: string): string {
	return `'${s.replaceAll("'", `'\\''`)}'`;
}

/** The `/rematch-*` commands: each runs the binary at expansion (`!` lines), before the model sees it. */
export function commands(bin: string): Record<string, { template: string; description: string }> {
	const b = shq(bin);
	const show = (what: string) => `The output above is \`${what}\`. Show it to the user as it is, without running anything.`;
	const cmd = (name: string, body: string, description: string, args = "") => ({
		description,
		template: `<rematch-command name="${name}"${args ? ` args="${args}"` : ""}/>\n${body}`,
	});
	return {
		"rematch-status": cmd(
			"status",
			`!\`${b} --text status\`\n\n!\`${b} --text skills sync --harness opencode\`\n\n${show("rematch status` and `rematch skills sync --harness opencode")}`,
			"rematch status: jobs, entries, and the skills linked for this directory",
		),
		"rematch-digest": cmd("digest", `!\`${b} --text digest\`\n\n${show("rematch digest")}`, "rematch digest: what changed and the approvals waiting"),
		"rematch-why": cmd("why", `!\`${b} --text library why $ARGUMENTS\`\n\n${show("rematch library why")}`, "rematch library why [oc:<session>]"),
		"rematch-library": cmd("library", `!\`${b} --text library ls\`\n\n${show("rematch library ls")}`, "rematch library ls: every entry with status and counters"),
		"rematch-approve": cmd(
			"approve",
			`!\`${b} --text library approve $ARGUMENTS\`\n\n${show("rematch library approve")}`,
			"rematch approve <id>: approve an entry's scripts or hook (yours only)",
		),
		"rematch-decline": cmd(
			"decline",
			`!\`${b} --text library decline $ARGUMENTS\`\n\n${show("rematch library decline")}`,
			"rematch decline <id> (yours only)",
		),
		"rematch-off": cmd("off", `!\`${b} --text off\`\n\n${show("rematch off")}`, "rematch off: the kill switch"),
		"rematch-on": cmd("on", `!\`${b} --text on\`\n\n!\`${b} --text skills sync --harness opencode\`\n\n${show("rematch on")}`, "rematch on"),
		"rematch-mark": cmd(
			"mark",
			"rematch recorded a mark ($ARGUMENTS) on the previous turn of this session. Acknowledge it in one short line and do nothing else.",
			"rematch mark correction|preference|scope: classify the last turn",
			"$ARGUMENTS",
		),
	};
}

export const RematchPlugin: Plugin = async (input: PluginInput) => {
	const root = rematchRoot();
	const bin = resolveBinary();
	const hardOff = process.env.REMATCH === "0";
	const isOff = () => hardOff || fs.existsSync(path.join(root, "off"));
	const sharedSkills = path.join(root, "skills");
	const interactive = HEADLESS[process.argv[2] ?? ""] !== true;
	const { client } = input;

	const log = (level: "warn" | "info" | "debug", message: string) => {
		client.app.log({ body: { service: "rematch", level, message } }).catch(() => {});
	};
	if (!bin) {
		log("warn", "rematch binary not found ($REMATCH_BIN, <checkout>/target/{release,debug}, PATH): not tracing");
		return {};
	}

	/** One bounded `rematch` call; never throws. */
	function call(argv: string[], stdin: Json | null, timeoutMs: number): Promise<Envelope> {
		const operation = argv[0] ?? "";
		const { promise, resolve } = Promise.withResolvers<Envelope>();
		let stdout = "";
		let stderr = "";
		let killed = false;
		let child: ChildProcess;
		try {
			child = spawn(bin as string, argv, { stdio: ["pipe", "pipe", "pipe"], cwd: input.directory });
		} catch (error) {
			resolve({ ok: false, operation, error: errorText(error) });
			return promise;
		}
		const timer = setTimeout(() => {
			killed = true;
			child.kill("SIGKILL");
		}, timeoutMs);
		child.stdout?.on("data", (d: Buffer) => (stdout += d.toString()));
		child.stderr?.on("data", (d: Buffer) => (stderr += d.toString()));
		child.on("error", error => {
			clearTimeout(timer);
			resolve({ ok: false, operation, error: errorText(error) });
		});
		child.on("close", code => {
			clearTimeout(timer);
			const env = decode({ stdout, stderr, code: code ?? 1, killed } as never, operation);
			if (!env.ok) log("warn", `${operation} failed: ${env.error}${stderr ? `: ${stderr.slice(0, 400)}` : ""}`);
			resolve(env);
		});
		child.stdin?.on("error", () => {});
		child.stdin?.end(stdin ? JSON.stringify(stdin) : "");
		return promise;
	}

	// --- what loads for this directory --------------------------------------------------------
	let callTools: Set<string> = new Set();
	let truncateRules: TruncateRule[] = [];
	/** An approved `retrieve` hook applies here: units go into each request's system prompt. */
	let retrieves = false;
	/** `retrieve` context per session for the current prompt (null: no units); dropped on new input and on every hook reload. */
	const retrieved = new Map<string, string | null>();
	let skillsPaths: string[] = [];

	function adopt(load: unknown) {
		if (!isRecord(load) || !Array.isArray(load.hooks)) return;
		const tools = new Set<string>();
		const rules: TruncateRule[] = [];
		for (const h of load.hooks.filter(isRecord)) {
			const tool = typeof h.tool === "string" ? h.tool : "";
			if (h.mode === "block" || h.mode === "stuck") tools.add(tool);
			const p = isRecord(h.params) ? h.params : {};
			if (h.mode === "truncate" && typeof p.max_age_turns === "number" && typeof p.keep_chars === "number") {
				rules.push({ tool, max_age_turns: p.max_age_turns, keep_chars: p.keep_chars });
			}
		}
		callTools = tools;
		truncateRules = rules;
		retrieves = load.hooks.some(h => isRecord(h) && h.mode === "retrieve");
		// Approvals, declines and the switch change what runs: no cached units survive them.
		retrieved.clear();
	}

	async function control(events: Json[], timeoutMs = 8000): Promise<Json[]> {
		if (isOff()) return [];
		const env = await call(
			["trace-event", "--harness", "opencode"],
			{ pid: process.pid, directory: input.directory, skills_paths: skillsPaths, events },
			timeoutMs,
		);
		return env.ok && isRecord(env.data) && Array.isArray(env.data.events) ? env.data.events.filter(isRecord) : [];
	}

	async function reload() {
		const [load] = await control([{ kind: "load" }], 5000);
		if (load && !load.error && !load.off) adopt(load);
		else if (load?.off) adopt({ hooks: [] });
	}

	// The shared dir is linked for this directory before OpenCode scans skills (lazily, after
	// plugin init); segments left open by a process that died are finalized first.
	if (!isOff()) {
		const results = await control([{ kind: "load" }, { kind: "reconcile" }], 8000);
		adopt(results.find(r => r.kind === "load"));
		// Reflection makes its model calls through omp; detached, one run at a time (the
		// binary's pending/.lock), never inside the OpenCode process.
		if (process.env.OMP_BIN || which("omp")) {
			try {
				spawn(bin, ["reflect", "--pending"], { detached: true, stdio: "ignore" }).unref();
			} catch (error) {
				log("warn", `reflect spawn failed: ${errorText(error)}`);
			}
		}
	}

	// --- sessions -----------------------------------------------------------------------------
	const sessions = new Map<string, SessionInfo>();
	const lookups = new Map<string, Promise<SessionInfo>>();
	const toInfo = (s: Json): SessionInfo => ({
		parent: typeof s.parentID === "string" && s.parentID ? s.parentID : null,
		directory: typeof s.directory === "string" && s.directory ? s.directory : null,
		version: typeof s.version === "string" ? s.version : null,
	});
	function sessionInfo(id: string): Promise<SessionInfo> {
		const known = sessions.get(id);
		if (known) return Promise.resolve(known);
		let p = lookups.get(id);
		if (!p) {
			p = client.session
				.get({ path: { id } })
				.then(r => {
					const info = isRecord(r.data) ? toInfo(r.data) : { parent: null, directory: null, version: null };
					sessions.set(id, info);
					return info;
				})
				.catch(() => ({ parent: null, directory: null, version: null }));
			lookups.set(id, p);
		}
		return p;
	}

	// Per-session ordered queues: events reach the binary in host order, batched while a call runs.
	type Queue = { items: Json[]; busy: Promise<void> | null };
	const queues = new Map<string, Queue>();
	const queueOf = (sid: string) => {
		let q = queues.get(sid);
		if (!q) {
			q = { items: [], busy: null };
			queues.set(sid, q);
		}
		return q;
	};
	function kick(sid: string, q: Queue) {
		if (q.busy || q.items.length === 0) return;
		const batch = q.items.splice(0);
		q.busy = (async () => {
			const info = await sessionInfo(sid);
			// Subagent sessions are not traced: their prompt is the parent agent's.
			if (info.parent || isOff()) return;
			const events = batch.map(e => ({ ...e, session: sid, directory: info.directory, version: info.version }));
			const results = await control(events, 10000);
			for (const r of results) if (typeof r.error === "string") log("warn", `${String(r.kind)}: ${r.error}`);
		})()
			.catch(error => log("warn", `trace-event failed: ${errorText(error)}`))
			.finally(() => {
				q.busy = null;
				kick(sid, q);
			});
	}
	function enqueue(sid: string, ev: Json) {
		const q = queueOf(sid);
		q.items.push(ev);
		// A new prompt or tool result changes the retrieval query: ask again before the next request.
		if (ev.kind === "prompt" || ev.kind === "tool") retrieved.delete(sid);
		kick(sid, q);
	}
	async function drain(sid: string) {
		const q = queues.get(sid);
		while (q?.busy) await q.busy;
	}

	// Streaming assistant text per message (part id → latest text), sent with its `turn`.
	const texts = new Map<string, Map<string, string>>();
	const sentTurns = new Set<string>();
	const sentTools = new Set<string>();
	const pendingCommand = new Map<string, { name: string; arguments: string }>();
	let digestShown = false;
	let lastReconcile = Date.now();

	async function showDigest() {
		const [d] = await control([{ kind: "digest" }], 8000);
		const toast = isRecord(d?.digest) ? d.digest.toast : null;
		if (typeof toast !== "string" || !toast) return;
		await client.tui.showToast({ body: { title: "rematch", message: toast, variant: "info", duration: 15000 } }).catch(() => {});
	}

	const hooks: Hooks = {
		config: async cfg => {
			try {
				const paths = cfg.skills?.paths ?? [];
				if (!paths.includes(sharedSkills)) cfg.skills = { ...(cfg.skills ?? {}), paths: [...paths, sharedSkills] };
				skillsPaths = [...(cfg.skills?.paths ?? [])];
				const mine = commands(bin);
				const existing = cfg.command ?? {};
				const added: Record<string, unknown> = {};
				for (const [name, def] of Object.entries(mine)) if (!(name in existing)) added[name] = def;
				cfg.command = { ...existing, ...added } as typeof cfg.command;
			} catch (error) {
				log("warn", `config hook failed: ${errorText(error)}`);
			}
		},

		event: async ({ event }) => {
			try {
				const e = event as { type: string; properties: Json };
				const p = isRecord(e.properties) ? e.properties : {};
				switch (e.type) {
					case "session.created":
					case "session.updated":
						if (isRecord(p.info) && typeof p.info.id === "string") sessions.set(p.info.id, toInfo(p.info));
						return;
					case "message.part.updated": {
						const part = isRecord(p.part) ? p.part : null;
						if (!part || typeof part.sessionID !== "string") return;
						if (part.type === "text" && part.synthetic !== true && typeof part.messageID === "string" && typeof part.id === "string") {
							let m = texts.get(part.messageID);
							if (!m) texts.set(part.messageID, (m = new Map()));
							m.set(part.id, typeof part.text === "string" ? part.text : "");
							return;
						}
						const state = isRecord(part.state) ? part.state : null;
						if (part.type === "tool" && state && (state.status === "completed" || state.status === "error") && typeof part.id === "string") {
							if (sentTools.has(part.id)) return;
							sentTools.add(part.id);
							enqueue(part.sessionID, { kind: "tool", part });
						}
						return;
					}
					case "message.updated": {
						const info = isRecord(p.info) ? p.info : null;
						if (!info || info.role !== "assistant" || typeof info.id !== "string" || typeof info.sessionID !== "string") return;
						const done = isRecord(info.time) && typeof info.time.completed === "number";
						if (!done && !info.error) return;
						if (sentTurns.has(info.id)) return;
						sentTurns.add(info.id);
						const text = [...(texts.get(info.id)?.values() ?? [])].join("\n");
						texts.delete(info.id);
						enqueue(info.sessionID, { kind: "turn", info, text });
						return;
					}
					case "session.idle": {
						if (typeof p.sessionID !== "string") return;
						enqueue(p.sessionID, { kind: "idle", t: Date.now() });
						// Segments of crashed processes and quiet sessions are settled now and then.
						if (Date.now() - lastReconcile > 10 * 60 * 1000) {
							lastReconcile = Date.now();
							void control([{ kind: "reconcile" }]);
						}
						return;
					}
				}
			} catch (error) {
				log("warn", `event handler failed: ${errorText(error)}`);
			}
		},

		"command.execute.before": async inp => {
			pendingCommand.set(inp.sessionID, { name: inp.command, arguments: inp.arguments });
			// The command's `!` lines already ran: an approval or the switch changes what runs.
			if (["rematch-approve", "rematch-decline", "rematch-on", "rematch-off"].includes(inp.command)) await reload();
		},

		"chat.message": async (inp, out) => {
			try {
				const sid = inp.sessionID;
				const command = pendingCommand.get(sid) ?? null;
				pendingCommand.delete(sid);
				enqueue(sid, { kind: "prompt", message: out.message, parts: out.parts, command, agent: inp.agent ?? null });
				if (interactive && !digestShown && !isOff()) {
					digestShown = true;
					void sessionInfo(sid).then(info => (info.parent ? (digestShown = false) : showDigest()));
				}
			} catch (error) {
				log("warn", `chat.message failed: ${errorText(error)}`);
			}
		},

		"tool.execute.before": async (inp, out) => {
			const args = isRecord(out.args) ? out.args : {};
			if (inp.tool === "bash" && isApprovalCommand(args.command)) {
				throw new Error(
					"rematch: approving, declining and switching rematch on or off is the user's decision. Ask the user to run /rematch-approve <id>, /rematch-decline <id>, /rematch-on or /rematch-off.",
				);
			}
			if (isOff() || callTools.size === 0 || !(callTools.has("*") || callTools.has(inp.tool))) return;
			let verdict: Envelope;
			try {
				const info = await sessionInfo(inp.sessionID);
				await drain(inp.sessionID);
				verdict = await call(
					["hook-check", "--harness", "opencode"],
					{
						pid: process.pid,
						directory: info.directory ?? input.directory,
						session: inp.sessionID,
						parent: info.parent,
						tool: inp.tool,
						call_id: inp.callID,
						args,
					},
					5000,
				);
			} catch (error) {
				log("warn", `hook-check failed open: ${errorText(error)}`);
				return;
			}
			// Only an approved rule's intentional block stops the call; a failed check lets it run.
			if (verdict.ok && isRecord(verdict.data) && verdict.data.block === true) {
				throw new Error(typeof verdict.data.reason === "string" ? verdict.data.reason : "blocked by a rematch hook");
			}
		},

		"experimental.chat.messages.transform": async (_inp, out) => {
			try {
				if (truncateRules.length > 0 && !isOff()) truncateToolOutputs(out.messages as unknown as OcMessage[], truncateRules);
			} catch (error) {
				log("warn", `truncate failed open: ${errorText(error)}`);
			}
		},

		// `retrieve`: OpenCode calls this before every model request with the session id and
		// sends `system` as that request's system prompt; nothing here is stored in the session.
		// The binary ranks the loaded packages' units against the trace's last prompt and tool
		// paths and records `retrieval` when the unit set changes (attribution). Bounded (3 s),
		// cached until the next prompt or tool result, fails open.
		"experimental.chat.system.transform": async (inp, out) => {
			try {
				const sid = inp.sessionID;
				if (!retrieves || !sid || isOff()) return;
				const info = await sessionInfo(sid);
				if (info.parent) return;
				let context = retrieved.get(sid);
				if (context === undefined) {
					// The prompt's `chat.message` was enqueued before this request was prepared: draining
					// puts it in the trace, so the query is this prompt's.
					await drain(sid);
					const [r] = await control([{ kind: "retrieve", session: sid, directory: info.directory, version: info.version }], 3000);
					const value = retrievalCacheValue(r);
					if (value !== undefined) retrieved.set(sid, value);
					context = value ?? null;
				}
				if (context) out.system.push(context);
			} catch (error) {
				log("warn", `retrieve failed open: ${errorText(error)}`);
			}
		},

		tool: {
			rematch: {
				description:
					"Read rematch's state (the skill/hook library that learns from your sessions): status, doctor, library-ls, library-why [oc:<session>], match, builtins-ls. Read-only: approving, declining, and on/off are the user's commands.",
				args: {
					operation: { type: "string", enum: Object.keys(TOOL_OPERATIONS), description: "What to read" },
					arguments: { type: "array", items: { type: "string" }, description: "Extra arguments (library-why: an entry id or session id); [] for none" },
				} as never,
				async execute(raw: unknown, ctx: { directory?: string }) {
					const a = isRecord(raw) ? raw : {};
					const op = typeof a.operation === "string" ? TOOL_OPERATIONS[a.operation] : undefined;
					if (!op) return `unknown operation; one of ${Object.keys(TOOL_OPERATIONS).join(", ")}`;
					const extra = Array.isArray(a.arguments) ? a.arguments.filter((x): x is string => typeof x === "string" && !x.startsWith("-")) : [];
					const argv = [...op, ...(a.operation === "library-why" ? extra.slice(0, 1) : [])];
					if (a.operation === "match") argv.push("--cwd", ctx.directory ?? input.directory);
					const env = await call(argv, null, 15000);
					return JSON.stringify(env.ok ? env.data : { error: env.error }, null, 1).slice(0, 12000);
				},
			},
		} as never,

		dispose: async () => {
			try {
				await Promise.all([...queues.keys()].map(drain));
				await control([{ kind: "shutdown" }], 4000);
			} catch (error) {
				log("warn", `shutdown failed: ${errorText(error)}`);
			}
		},
	};
	return hooks;
};

export default { id: "rematch", server: RematchPlugin };
