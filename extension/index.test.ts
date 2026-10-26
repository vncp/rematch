// Hook runtime of the rematch extension (DESIGN §10): the pure mode helpers, and the factory against
// a fake host whose `pi.exec` answers for the binary. Run: `bun test extension`.
import { afterAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import rematch, {
	CallHistory,
	callKey,
	changesLoaded,
	hookFromModule,
	loadedEntryIds,
	loadHook,
	RETRIEVAL_TYPE,
	retrievalMessage,
	retrievalQuery,
	stuckReason,
	truncateToolResults,
	truncationMarker,
} from "./index.ts";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rematch-ext-test-"));
afterAll(() => fs.rmSync(tmp, { recursive: true, force: true }));

const stuck = (repeats = 3) => hookFromModule({ id: "stuck-1", mode: "stuck", tool: "*", message: "stuck", params: { repeats } }, "x");
const truncate = (max_age_turns: number, keep_chars: number, tool = "*") =>
	hookFromModule({ id: "trunc-1", mode: "truncate", tool, message: "", params: { max_age_turns, keep_chars } }, "x");
const user = (text: string, synthetic = false) => ({ role: "user", content: text, ...(synthetic ? { synthetic: true } : {}) });
const result = (text: string, extra: object[] = [], toolName = "bash") => ({
	role: "toolResult",
	toolName,
	toolCallId: "c",
	content: [{ type: "text", text }, ...extra],
});

describe("hookFromModule", () => {
	test("accepts each mode with its numeric params", () => {
		const b = hookFromModule({ id: "b", mode: "block", tool: "bash", field: "command", message: "no", params: { source: "rm -rf", flags: "i" } }, "f");
		expect(b.pattern?.flags).toBe("i");
		expect(b.pattern?.source).toBe("rm -rf");
		expect(stuck().params).toEqual({ repeats: 3 });
		expect(truncate(6, 1200).params).toEqual({ max_age_turns: 6, keep_chars: 1200 });
		const r = hookFromModule({ mode: "retrieve", tool: "*", params: { budget_bytes: 6000, min_overlap: 2, extra: 1 } }, "fallback");
		expect(r.id).toBe("fallback");
		expect(r.params).toEqual({ budget_bytes: 6000, min_overlap: 2 });
	});

	test("rejects what the binary would not have written", () => {
		expect(() => hookFromModule(undefined, "f")).toThrow("no default export");
		expect(() => hookFromModule({ mode: "warn", tool: "*" }, "f")).toThrow("unknown hook mode");
		expect(() => hookFromModule({ mode: "stuck", params: { repeats: 3 } }, "f")).toThrow("names no tool");
		expect(() => hookFromModule({ mode: "stuck", tool: "*", params: {} }, "f")).toThrow("params.repeats");
		expect(() => hookFromModule({ mode: "truncate", tool: "*", params: { max_age_turns: 1, keep_chars: -1 } }, "f")).toThrow("params.keep_chars");
		expect(() => hookFromModule({ mode: "retrieve", tool: "*", params: { budget_bytes: "6000", min_overlap: 2 } }, "f")).toThrow("params.budget_bytes");
		expect(() => hookFromModule({ mode: "block", tool: "bash", params: { source: "x", flags: "" } }, "f")).toThrow("names no field");
		expect(() => hookFromModule({ mode: "block", tool: "bash", field: "command", params: {} }, "f")).toThrow("params.source");
	});
});

describe("loadHook", () => {
	test("a rewritten module file is re-imported, not served from the module cache", async () => {
		const file = path.join(tmp, "hook-reload.ts");
		fs.writeFileSync(file, `export default { id: "hook-reload", mode: "stuck", tool: "*", message: "a", params: { repeats: 3 } };\n`);
		expect((await loadHook(file)).params.repeats).toBe(3);
		fs.writeFileSync(file, `export default { id: "hook-reload", mode: "stuck", tool: "*", message: "b", params: { repeats: 5 } };\n`);
		const later = new Date(Date.now() + 5000);
		fs.utimesSync(file, later, later);
		const again = await loadHook(file);
		expect([again.message, again.params.repeats]).toEqual(["b", 5]);
	});
});

describe("CallHistory + stuckReason", () => {
	const hook = stuck(3);
	const verdicts = (keys: string[]) => {
		const calls = new CallHistory();
		return keys.map(k => {
			const reason = stuckReason(hook, "bash", calls.run(k));
			calls.push(k);
			return reason;
		});
	};

	test("the first call of a session passes", () => {
		expect(new CallHistory().run(callKey("bash", { command: "ls" }))).toBe(0);
		expect(verdicts(["a"])).toEqual([null]);
	});

	test("the 3rd identical call passes, the 4th and later block", () => {
		expect(verdicts(["a", "a", "a", "a", "a"])).toEqual([null, null, null, "stuck", "stuck"]);
	});

	test("edit → test → edit → test never blocks", () => {
		const edit = callKey("edit", { path: "a.rs" });
		const test = callKey("bash", { command: "cargo test" });
		expect(verdicts([edit, test, edit, test, edit, test, edit, test]).every(r => r === null)).toBe(true);
	});

	test("the key is the tool plus the canonical args hash", () => {
		expect(callKey("bash", { a: 1, b: 2 })).toBe(callKey("bash", { b: 2, a: 1 }));
		expect(callKey("bash", { a: 1 })).not.toBe(callKey("read", { a: 1 }));
	});

	test("clear resets the run", () => {
		const calls = new CallHistory();
		for (let i = 0; i < 4; i++) calls.push("a");
		calls.clear();
		expect(calls.run("a")).toBe(0);
	});
});

describe("truncateToolResults", () => {
	const long = "x".repeat(40);

	test("the current turn's results (age 0) are untouched", () => {
		expect(truncateToolResults([user("q"), result(long)], [truncate(1, 5)])).toBeNull();
	});

	test("results at most max_age_turns old, or at most keep_chars long, are untouched", () => {
		expect(truncateToolResults([result(long), user("a")], [truncate(1, 5)])).toBeNull();
		expect(truncateToolResults([result("short"), user("a"), user("b")], [truncate(1, 5)])).toBeNull();
	});

	test("older results keep keep_chars plus a marker in a new array, the input unchanged", () => {
		const input = [result(long), user("a"), user("b")];
		const before = structuredClone(input);
		const out = truncateToolResults(input, [truncate(1, 5)]);
		expect(out).not.toBeNull();
		expect(out).not.toBe(input);
		expect(input).toEqual(before);
		expect(out?.[0]).toEqual({ ...input[0], content: [{ type: "text", text: `xxxxx\n${truncationMarker(35, 2)}` }] });
		expect(out?.[1]).toBe(input[1]);
	});

	test("synthetic user messages do not age a result", () => {
		expect(truncateToolResults([result(long), user("s", true), user("s", true), user("a")], [truncate(1, 5)])).toBeNull();
		expect(truncateToolResults([result(long), user("s", true), user("a"), user("b")], [truncate(1, 5)])).not.toBeNull();
	});

	test("image blocks are kept and the strictest matching rule wins", () => {
		const image = { type: "image", data: "AAAA", mimeType: "image/png" };
		const out = truncateToolResults([result(long, [image]), user("a"), user("b")], [truncate(1, 20), truncate(1, 5), truncate(0, 1, "read")]);
		expect(out?.[0]).toMatchObject({ content: [{ type: "text", text: `xxxxx\n${truncationMarker(35, 2)}` }, image] });
	});

	test("the cut never splits a surrogate pair", () => {
		const text = `abcd😀${"y".repeat(20)}`;
		const out = truncateToolResults([result(text), user("a"), user("b")], [truncate(1, 5)]);
		const cut = `abcd\n${truncationMarker(text.length - 4, 2)}`;
		expect(out?.[0]).toMatchObject({ content: [{ type: "text", text: cut }] });
	});
});

describe("retrievalQuery", () => {
	test("last real user text plus the deduplicated paths behind the last tool results", () => {
		const messages = [
			user("old request"),
			{
				role: "assistant",
				content: [
					{ type: "toolCall", id: "c1", name: "read", arguments: { path: "/r/src/a.rs:10-20" } },
					{ type: "toolCall", id: "c2", name: "edit", arguments: { file_path: "/r/src/b.rs" } },
					{ type: "toolCall", id: "c3", name: "read", arguments: { path: "/r/src/a.rs" } },
				],
			},
			{ role: "toolResult", toolCallId: "c1", content: [] },
			{ role: "toolResult", toolCallId: "c2", content: [] },
			{ role: "toolResult", toolCallId: "c3", content: [] },
			user("fix the ledger migration"),
			user("[system note]", true),
		];
		expect(retrievalQuery(messages)).toBe("fix the ledger migration\n/r/src/a.rs\n/r/src/b.rs");
		expect(retrievalQuery(messages, 1)).toBe("fix the ledger migration\n/r/src/a.rs");
		expect(retrievalQuery([])).toBe("");
	});
});

describe("retrievalMessage", () => {
	test("one hidden CustomMessage carrying every unit", () => {
		const units = [
			{ entry_id: "e1", name: "ledger", path: "units/migrate.md", title: "Migrate", text: "Run migrations first.\n\n" },
			{ entry_id: "e2", name: "facts", path: "facts/a.md", title: "A", text: "Fact." },
		];
		const m = retrievalMessage(units, 42);
		expect(Object.keys(m).sort()).toEqual(["content", "customType", "details", "display", "role", "timestamp"]);
		expect([m.role, m.customType, m.display, m.timestamp]).toEqual(["custom", RETRIEVAL_TYPE, false, 42]);
		expect(m.details).toEqual({ units: [{ entry_id: "e1", path: "units/migrate.md" }, { entry_id: "e2", path: "facts/a.md" }] });
		const content = m.content as string;
		expect(content.startsWith("[rematch] Units of skills loaded in this session")).toBe(true);
		expect(content).toContain('<unit skill="ledger" path="units/migrate.md">\nRun migrations first.\n</unit>\n\n<unit skill="facts" path="facts/a.md">\nFact.\n</unit>');
	});
});

describe("argv and match helpers", () => {
	test("approve, activate, disable and rollback change what loads; decline and reads do not", () => {
		expect(changesLoaded(["library", "approve", "e"])).toBe(true);
		expect(changesLoaded(["library", "disable", "e"])).toBe(true);
		expect(changesLoaded(["library", "rollback", "e"])).toBe(true);
		expect(changesLoaded(["activate", "e"])).toBe(true);
		expect(changesLoaded(["library", "decline", "e"])).toBe(false);
		expect(changesLoaded(["library", "ls"])).toBe(false);
		expect(changesLoaded(["status"])).toBe(false);
	});

	test("loaded entries are the predicate hits minus the retrieve hooks", () => {
		const retrieve = hookFromModule({ id: "r", mode: "retrieve", tool: "*", params: { budget_bytes: 10, min_overlap: 1 } }, "r");
		const data = { entries: [{ id: "a" }, { id: "r" }, { id: "s" }, { id: "miss", predicate_hit: false }, { name: "no id" }] };
		expect(loadedEntryIds(data, [retrieve, stuck()])).toEqual(["a", "s"]);
		expect(loadedEntryIds(undefined, [])).toEqual([]);
	});
});

// ---------------------------------------------------------------------------------------------
// Factory against a fake host: the binary is answered by `pi.exec`.

type Handler = (event: unknown, ctx: unknown) => unknown;

function writeHook(id: string, mode: string, params: object): string {
	const file = path.join(tmp, `${id}.ts`);
	fs.writeFileSync(file, `export default ${JSON.stringify({ id, mode, tool: "*", field: "", message: `${id} says no`, params })};\n`);
	return file;
}

type HostState = { entries: object[]; hooks: string[]; approvals?: object[]; onApprove?: () => void };

/** The `messages` a `context` handler returned (throws when it changed nothing). */
function contextMessages(r: unknown): unknown[] {
	if (!r || typeof r !== "object" || !("messages" in r) || !Array.isArray(r.messages)) throw new Error("context changed nothing");
	return r.messages;
}

/** The reason a `tool_call` handler blocked with, or undefined when it let the call run. */
function blockReasonOf(r: unknown): string | undefined {
	if (!r || typeof r !== "object" || !("reason" in r)) return undefined;
	return String(r.reason);
}

async function host(name: string, state: HostState) {
	const root = path.join(tmp, name);
	fs.mkdirSync(root, { recursive: true });
	// A distinct binary path per host keeps the module-level match cache from leaking between tests.
	const bin = path.join(root, "rematch");
	fs.writeFileSync(bin, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
	const saved = { REMATCH_BIN: process.env.REMATCH_BIN, REMATCH_ROOT: process.env.REMATCH_ROOT, REMATCH: process.env.REMATCH };
	process.env.REMATCH_BIN = bin;
	process.env.REMATCH_ROOT = root;
	delete process.env.REMATCH;
	const execs: string[][] = [];
	const handlers = new Map<string, Handler>();
	let command: Handler | null = null;
	let shortcut: Handler | null = null;
	const ok = (data: object) => ({ stdout: JSON.stringify({ ok: true, operation: "x", data }), stderr: "", code: 0, killed: false });
	const z: unknown = new Proxy(() => z, { get: () => z, apply: () => z });
	const pi = {
		zod: z,
		pi: { settings: {}, VERSION: "test" },
		logger: { warn() {}, debug() {} },
		async exec(cmd: string, argv: string[]) {
			if (cmd !== bin) return { stdout: "", stderr: "", code: 128, killed: false };
			execs.push(argv);
			switch (argv[0]) {
				case "match":
					return ok({ entries: state.entries, hooks: state.hooks, skill_dirs: [] });
				case "library":
					if (argv[1] === "approve") state.onApprove?.();
					return ok({});
				case "digest":
					return ok({ lines: [], approvals: state.approvals ?? [] });
				case "retrieve":
					return ok({ units: [{ entry_id: "pkg", name: "ledger", path: "units/a.md", title: "A", text: "Unit text." }] });
				default:
					return ok({});
			}
		},
		on(event: string, fn: Handler) {
			handlers.set(event, fn);
		},
		registerTool() {},
		registerCommand(_name: string, spec: { handler: Handler }) {
			command = spec.handler;
		},
		registerShortcut(_key: string, spec: { handler: (ctx: unknown) => unknown }) {
			shortcut = (_event, ctx) => spec.handler(ctx);
		},
		appendEntry() {},
		sendMessage() {},
	};
	try {
		await rematch(pi as never);
	} finally {
		for (const [k, v] of Object.entries(saved)) {
			if (v === undefined) delete process.env[k];
			else process.env[k] = v;
		}
	}
	let session = `${name}-s1`;
	const ctx = {
		cwd: process.cwd(),
		agent: { kind: "main", name: "main" },
		hasUI: false,
		mode: "interactive",
		model: { provider: "p", id: "m" },
		isIdle: () => true,
		sessionManager: { getSessionId: () => session },
		ui: { notify() {}, async confirm() { return true; } },
	};
	const fire = (event: string, payload: unknown = {}) => handlers.get(event)?.(payload, ctx);
	const traceOf = () =>
		fs
			.readFileSync(path.join(root, "traces", `${session}.jsonl`), "utf8")
			.trim()
			.split("\n")
			.map((l): Record<string, unknown> => JSON.parse(l));
	return {
		ctx,
		execs,
		fire,
		traceOf,
		command: (raw: string) => command?.(raw, ctx),
		digest: () => shortcut?.(undefined, ctx),
		toolCall: (input: object) => blockReasonOf(fire("tool_call", { toolName: "bash", toolCallId: "t", input })),
		switchTo(id: string) {
			session = id;
		},
		matches: () => execs.filter(a => a[0] === "match").map(a => a[2]),
	};
}

describe("factory", () => {
	test("retrieve: no --entries, credited by its retrieval record only, one stable message", async () => {
		const retrieveFile = writeHook("hook-retrieve", "retrieve", { budget_bytes: 6000, min_overlap: 2 });
		const h = await host("retrieve", { entries: [{ id: "hook-retrieve", predicate_hit: true }, { id: "pkg", predicate_hit: true }], hooks: [retrieveFile] });
		await h.fire("session_start");
		const start = h.traceOf()[0];
		expect(start.ev).toBe("session_start");
		expect(start.loaded_entries).toEqual(["pkg"]);
		expect(start.hooks_loaded).toEqual(["hook-retrieve"]);

		const messages = [user("fix the ledger migration")];
		const first = contextMessages(await h.fire("context", { messages })).at(-1);
		const argv = h.execs.find(a => a[0] === "retrieve") ?? [];
		expect(argv).not.toContain("--entries");
		expect(argv.slice(argv.indexOf("--query"))).toEqual(["--query", "fix the ledger migration"]);
		expect(first).toMatchObject({ role: "custom", customType: RETRIEVAL_TYPE });
		// The cached answer re-sends the very same message object.
		expect(contextMessages(await h.fire("context", { messages })).at(-1)).toBe(first);
		expect(h.execs.filter(a => a[0] === "retrieve")).toHaveLength(1);

		const retrievals = h.traceOf().filter(r => r.ev === "retrieval");
		expect(retrievals).toHaveLength(1);
		expect(retrievals[0].entry_ids).toEqual(["hook-retrieve"]);
	});

	const stuckFile = () => writeHook("hook-stuck", "stuck", { repeats: 3 });
	const approvable = (): HostState => {
		const state: HostState = { entries: [], hooks: [], approvals: [{ id: "hook-stuck", name: "stuck-detector", kind: "hook" }] };
		state.onApprove = () => {
			state.hooks = [stuckFile()];
			state.entries = [{ id: "hook-stuck", predicate_hit: true }];
		};
		return state;
	};
	const same = { command: "cargo test" };

	test("a hook approved with /rematch approve runs at once", async () => {
		const h = await host("approve", approvable());
		await h.fire("session_start");
		for (let i = 0; i < 5; i++) expect(h.toolCall(same)).toBeUndefined();
		await h.command("approve hook-stuck");
		expect(h.matches()).toHaveLength(2);
		// The run starts with the approval: the 3rd identical call still passes, the 4th blocks.
		expect([h.toolCall(same), h.toolCall(same), h.toolCall(same), h.toolCall(same)]).toEqual([undefined, undefined, undefined, "hook-stuck says no"]);
	});

	test("a hook approved in the digest runs at once", async () => {
		const h = await host("digest", approvable());
		await h.fire("session_start");
		h.ctx.hasUI = true;
		await h.digest();
		expect(h.execs.some(a => a[0] === "library" && a[1] === "approve" && a[2] === "hook-stuck")).toBe(true);
		expect(h.matches()).toHaveLength(2);
		expect([h.toolCall(same), h.toolCall(same), h.toolCall(same), h.toolCall(same)]).toEqual([undefined, undefined, undefined, "hook-stuck says no"]);
	});

	test("a call a block rule rejected never ran: it does not break a stuck run", async () => {
		const blockFile = path.join(tmp, "hook-block.ts");
		const rule = { id: "hook-block", mode: "block", tool: "bash", field: "command", message: "blocked", params: { source: "^rm ", flags: "" } };
		fs.writeFileSync(blockFile, `export default ${JSON.stringify(rule)};\n`);
		const h = await host("block", { entries: [], hooks: [blockFile, stuckFile()] });
		await h.fire("session_start");
		expect([h.toolCall(same), h.toolCall(same), h.toolCall(same)]).toEqual([undefined, undefined, undefined]);
		expect(h.toolCall({ command: "rm -rf target" })).toBe("blocked");
		expect(h.toolCall(same)).toBe("hook-stuck says no");
	});

	test("a session switch and a cwd change re-run match", async () => {
		const h = await host("rotate", { entries: [], hooks: [] });
		await h.fire("session_start");
		expect(h.matches()).toEqual([process.cwd()]);
		h.switchTo("rotate-s2");
		await h.fire("session_switch");
		expect(h.matches()).toEqual([process.cwd(), process.cwd()]);
		expect(h.traceOf()[0].session_id).toBe("rotate-s2");
		h.ctx.cwd = tmp;
		await h.fire("context", { messages: [user("q")] });
		expect(h.matches()).toEqual([process.cwd(), process.cwd(), tmp]);
		await h.fire("context", { messages: [user("q")] });
		expect(h.matches()).toHaveLength(3);
	});
});
