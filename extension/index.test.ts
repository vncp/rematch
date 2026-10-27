// Hook runtime of the rematch extension (DESIGN §10): the pure mode helpers, and the factory against
// a fake host whose `pi.exec` answers for the binary and whose settings registry records overrides.
// Run: `bun test extension`.
import { afterAll, describe, expect, mock, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import rematch, {
	appendPromptFragments,
	CallHistory,
	callKey,
	changedPaths,
	changesLoaded,
	FILES_CHANGED_MAX,
	filesChanged,
	hookFromModule,
	loadedEntriesOf,
	loadHook,
	promptFragmentsOf,
	renderPromptFragments,
	RETRIEVAL_TYPE,
	REVIEW_APPROVE,
	REVIEW_DECLINE,
	REVIEW_INSPECT,
	REVIEW_LATER,
	reviewChoices,
	reviewTarget,
	retrievalMessage,
	retrievalQuery,
	sessionSkillDirs,
	shadowedByCustomDirs,
	stuckReason,
	subpathOf,
	truncateToolResults,
	truncationMarker,
	withPromptBlock,
} from "./index.ts";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rematch-ext-test-"));
afterAll(() => fs.rmSync(tmp, { recursive: true, force: true }));

/** Every runtime override the factory made: `[settings scope, setting id, value]` (a cleared override is not listed). */
const overrides: [unknown, string, string[]][] = [];
/** What the user configured, per setting id (the persisted layer the fake registry returns under an override). */
const persisted: Record<string, string[]> = { "skills.customDirectories": ["/configured/skills"], "task.disabledAgents": ["users-own-off"] };
mock.module("@oh-my-pi/pi-coding-agent/config/registry", () => ({
	lookup: (key: string) =>
		key in persisted
			? {
					get: (scope: Record<string, string[] | undefined>) => scope[key] ?? persisted[key],
					override: (scope: Record<string, string[] | undefined>, value: string[] | undefined) => {
						scope[key] = value;
						if (value !== undefined) overrides.push([scope, key, value]);
					},
				}
			: undefined,
}));
const overridesOf = (scope: unknown, key: string) => overrides.filter(([s, k]) => s === scope && k === key).map(([, , v]) => v);

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

	test("loaded entries are the predicate hits minus the retrieve hooks, versioned", () => {
		const retrieve = hookFromModule({ id: "r", mode: "retrieve", tool: "*", params: { budget_bytes: 10, min_overlap: 1 } }, "r");
		const data = { entries: [{ id: "a", version: 2, sha: "ab" }, { id: "r" }, { id: "s", version: 1, sha: "" }, { id: "miss", predicate_hit: false }, { name: "no id" }] };
		expect(loadedEntriesOf(data, [retrieve, stuck()])).toEqual([{ id: "a", version: 2, sha: "ab" }, { id: "s", version: 1 }]);
		expect(loadedEntriesOf(undefined, [])).toEqual([]);
	});
});

describe("reviewChoices", () => {
	const described = (kind: string, approve = true, decline = true) => {
		const id = `${kind}:x`;
		const argv = (verb: string) => ["library", verb, id, "--sha", "a".repeat(64)];
		const a = { id, kind, name: "x", summary: "", state: "pending", blocked: null, reload: null, approve: approve ? argv("approve") : null, decline: decline ? argv("decline") : null };
		return Object.fromEntries(reviewChoices(a).map(c => [c.label, c.description]));
	};

	test("a conflict offers restore and keep-mine, each with the bound command behind it", () => {
		const c = described("instructions_conflict");
		expect(c[REVIEW_APPROVE]).toBe("restore rematch's text over your edit");
		expect(c[REVIEW_DECLINE]).toBe("keep your edited text as your own; rematch stops managing it");
		expect(c[REVIEW_LATER]).toBeDefined();
	});

	test("Decline says what it does per kind; only entry parts retire the entry", () => {
		expect(described("instructions_applied", false)[REVIEW_DECLINE]).toContain("withdraws this text");
		expect(described("instructions")[REVIEW_DECLINE]).toBe("this text is not applied; the entry stays");
		expect(described("export")[REVIEW_DECLINE]).toContain("the entry stays");
		expect(described("entry")[REVIEW_DECLINE]).toBe("retires the entry");
		expect(described("instructions_applied", false)[REVIEW_APPROVE]).toBeUndefined();
	});
});

describe("identity and changed files (ADR 001)", () => {
	test("mutating calls name their files, patch payloads included", () => {
		expect(changedPaths("write", { path: "src/a.rs:1-5" })).toEqual(["src/a.rs"]);
		expect(changedPaths("Edit", { file_path: "/w/b.rs" })).toEqual(["/w/b.rs"]);
		expect(changedPaths("edit", { input: "[src/c.rs#AB12]\nreplace\n[src/d.rs#CD34]\n" })).toEqual(["src/c.rs", "src/d.rs"]);
		expect(changedPaths("apply_patch", { input: "*** Update File: e.rs\n*** Move to: f.rs\n" })).toEqual(["e.rs", "f.rs"]);
		expect(changedPaths("read", { path: "src/a.rs" })).toEqual([]);
		expect(changedPaths("write", { path: "skill://x" })).toEqual([]);
	});

	test("files_changed: bounded, repo-relative, null over budget", () => {
		const repo = fs.mkdtempSync(path.join(tmp, "repo-"));
		fs.mkdirSync(path.join(repo, "src"));
		fs.writeFileSync(path.join(repo, "src", "a.rs"), "fn a() {}");
		fs.writeFileSync(path.join(repo, "big.bin"), Buffer.alloc(1024 * 1024 + 1));
		const got = filesChanged(["src/a.rs", "big.bin", "gone.rs"], repo, repo, Date.now() + 5000);
		expect(got?.map(([p]) => p)).toEqual(["src/a.rs"]);
		expect(got?.[0][1]).toMatch(/^[0-9a-f]{64}$/);
		expect(filesChanged(["src/a.rs"], repo, repo, Date.now() - 1)).toBeNull();
		const many = Array.from({ length: 60 }, () => "src/a.rs");
		expect(filesChanged(many, repo, null, Date.now() + 5000)).toHaveLength(FILES_CHANGED_MAX);
		expect(filesChanged(["src/a.rs"], repo, null, Date.now() + 5000)?.[0][0]).toBe(path.join(repo, "src", "a.rs"));
	});

	test("subpath is toplevel-relative, empty at the top, null outside", () => {
		expect(subpathOf("/w/repo", "/w/repo/crates/x")).toBe("crates/x");
		expect(subpathOf("/w/repo", "/w/repo")).toBe("");
		expect(subpathOf("/w/repo", "/elsewhere")).toBeNull();
	});
});

// ---------------------------------------------------------------------------------------------
// Factory against a fake host: the binary is answered by `pi.exec`.

type Handler = (event: unknown, ctx: unknown) => unknown;
type ToolExecute = (id: string, params: unknown, signal: unknown, onUpdate: unknown, ctx: unknown) => Promise<unknown>;

function writeHook(id: string, mode: string, params: object): string {
	const file = path.join(tmp, `${id}.ts`);
	fs.writeFileSync(file, `export default ${JSON.stringify({ id, mode, tool: "*", field: "", message: `${id} says no`, params })};\n`);
	return file;
}

type HostState = {
	entries: object[];
	hooks: string[];
	dirs?: string[];
	visible?: string[];
	/** `skills sync` view key, links (`name → dir`) and the agents it withholds; `syncFails` makes it fail. */
	view?: string;
	links?: Record<string, string>;
	withheld?: string[];
	syncFails?: boolean;
	/** `digest.actions` (persistent, whatever `digest.last` says). */
	actions?: object[];
	onApprove?: () => void;
	/** `match` reports `profile_present` (REMATCH_ROOT/profile/.git exists). */
	profilePresent?: boolean;
	/** `match` reports `prompt_fragments` (approved personal AGENTS.fragment.md of the loaded entries). */
	fragments?: unknown;
	/** `match.instructions` (the personal AGENTS.md sections omp loads). */
	instructions?: unknown;
	/** Answers for `git rev-parse --show-toplevel --verify -q HEAD` and `git remote get-url origin`. */
	git?: { top: string; head?: string; remote?: string };
	/** What each review dialog answers, in order (undefined = Esc). */
	choices?: (string | undefined)[];
};

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
	const statuses: (string | undefined)[] = [];
	const titles: string[] = [];
	const handlers = new Map<string, Handler>();
	let command: Handler | null = null;
	let shortcut: Handler | null = null;
	let tool: ToolExecute | null = null;
	const ok = (data: object) => ({ stdout: JSON.stringify({ ok: true, operation: "x", data }), stderr: "", code: 0, killed: false });
	const z: unknown = new Proxy(() => z, { get: () => z, apply: () => z });
	const pi = {
		zod: z,
		pi: { settings: {} as Record<string, string[] | undefined>, VERSION: "test" },
		logger: { warn() {}, debug() {} },
		async exec(cmd: string, argv: string[]) {
			if (cmd === "git" && state.git) {
				const g = state.git;
				if (argv[0] === "rev-parse") return { stdout: g.head ? `${g.top}\n${g.head}\n` : `${g.top}\n`, stderr: "", code: g.head ? 0 : 1, killed: false };
				if (argv[0] === "remote") return g.remote ? { stdout: `${g.remote}\n`, stderr: "", code: 0, killed: false } : { stdout: "", stderr: "error: No such remote 'origin'", code: 2, killed: false };
			}
			if (cmd !== bin) return { stdout: "", stderr: "", code: 128, killed: false };
			execs.push(argv);
			switch (argv[0]) {
				case "match":
					return ok({ entries: state.entries, hooks: state.hooks, profile_present: state.profilePresent === true, prompt_fragments: state.fragments, instructions: state.instructions });
				case "skills":
					if (state.syncFails) return { stdout: JSON.stringify({ ok: false, error: "boom" }), stderr: "", code: 1, killed: false };
					return ok({ added: [], pruned: [], total: 0, dirs: state.dirs ?? [], visible: state.visible ?? [], off: false, view: state.view, links: state.links ?? {}, agents: { exposed: [], withheld: state.withheld ?? [], shadowed: [] } });
				case "library":
					if (argv[1] === "approve") state.onApprove?.();
					return ok({ lines: [`inspected ${argv[2]}`], action: (state.actions ?? []).find(a => "id" in a && a.id === argv[2]) });
				case "digest":
					return ok({ lines: [], actions: state.actions ?? [] });
				case "retrieve":
					return ok({ units: [{ entry_id: "pkg", name: "ledger", path: "units/a.md", title: "A", text: "Unit text." }] });
				default:
					return ok({});
			}
		},
		on(event: string, fn: Handler) {
			handlers.set(event, fn);
		},
		registerTool(spec: { execute: ToolExecute }) {
			tool = spec.execute;
		},
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
		ui: {
			notify() {},
			setStatus(_key: string, text: string | undefined) {
				statuses.push(text);
			},
			async select(title: string) {
				titles.push(title);
				return (state.choices ?? []).shift();
			},
			async confirm() {
				throw new Error("a review never asks yes/no: Esc must not decline");
			},
		},
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
		/** The `rematch` tool as the model calls it: its envelope. */
		async tool(operation: string, args: string[]) {
			const r = (await tool?.("t", { operation, arguments: args }, undefined, () => {}, ctx)) as { details: { ok: boolean; error?: string } };
			return r.details;
		},
		digest: () => shortcut?.(undefined, ctx),
		toolCall: (input: object) => blockReasonOf(fire("tool_call", { toolName: "bash", toolCallId: "t", input })),
		switchTo(id: string) {
			session = id;
		},
		matches: () => execs.filter(a => a[0] === "match").map(a => a[2]),
		settings: pi.pi.settings,
		statuses,
		titles,
		root,
	};
}

describe("factory", () => {
	test("retrieve: no --entries, credited by its retrieval record only, one stable message", async () => {
		const retrieveFile = writeHook("hook-retrieve", "retrieve", { budget_bytes: 6000, min_overlap: 2 });
		const h = await host("retrieve", { entries: [{ id: "hook-retrieve", predicate_hit: true }, { id: "pkg", predicate_hit: true }], hooks: [retrieveFile] });
		await h.fire("session_start");
		const start = h.traceOf()[0];
		expect(start.ev).toBe("session_start");
		expect(start.loaded_entries).toEqual([{ id: "pkg" }]);
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
	const approvable = (choices: (string | undefined)[] = []): HostState => {
		const action = {
			id: "hook-stuck",
			name: "stuck-detector",
			kind: "hook",
			state: "pending",
			summary: "approve hook stuck-detector",
			blocked: null,
			commands: { approve_argv: ["library", "approve", "hook-stuck", "--sha", "abc123"], decline_argv: ["library", "decline", "hook-stuck"] },
		};
		const state: HostState = { entries: [], hooks: [], actions: [action], choices };
		state.onApprove = () => {
			state.hooks = [stuckFile()];
			state.entries = [{ id: "hook-stuck", predicate_hit: true }];
			state.actions = [];
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

	test("/rematch approve <id> typed without a hash opens that action's dialog, which carries it", async () => {
		const h = await host("typed", approvable([REVIEW_APPROVE]));
		await h.fire("session_start");
		h.ctx.hasUI = true;
		await h.command("approve hook-stuck");
		expect(h.execs.filter(a => a[0] === "library")).toEqual([["library", "inspect", "hook-stuck"], ["library", "approve", "hook-stuck", "--sha", "abc123"]]);
		expect(h.titles).toHaveLength(1);
		expect(h.matches()).toHaveLength(2);
		expect(reviewTarget(["approve", "e1"])).toBe("e1");
		expect(reviewTarget(["decline", "e1"])).toBe("e1");
		expect(reviewTarget(["approve", "e1", "--sha", "abc"]), "a typed hash goes to the binary").toBeNull();
		expect(reviewTarget(["approve", "e1", "--sha=abc"])).toBeNull();
		expect(reviewTarget(["inspect", "e1"])).toBeNull();
		expect(reviewTarget(["approve"])).toBeNull();
	});

	test("a hook approved in the review runs at once, with the exact hash shown", async () => {
		const h = await host("digest", approvable([REVIEW_APPROVE]));
		await h.fire("session_start");
		h.ctx.hasUI = true;
		await h.digest();
		expect(h.execs.filter(a => a[0] === "library")).toEqual([["library", "approve", "hook-stuck", "--sha", "abc123"]]);
		expect(h.matches()).toHaveLength(2);
		expect([h.toolCall(same), h.toolCall(same), h.toolCall(same), h.toolCall(same)]).toEqual([undefined, undefined, undefined, "hook-stuck says no"]);
		expect(h.statuses.at(-1), "nothing waits any more").toBeUndefined();
	});

	test("session start only shows what waits; Esc and Later change nothing, and it stays pending", async () => {
		const h = await host("later", approvable([undefined, REVIEW_LATER, REVIEW_INSPECT, REVIEW_DECLINE]));
		h.ctx.hasUI = true;
		await h.fire("session_start");
		await h.digest(); // Esc
		expect(h.titles, "one dialog: the review's, none from the session start").toHaveLength(1);
		expect(h.statuses.length >= 2 && h.statuses.every(s => s === "rematch: 1 waiting · /rematch review"), "start and digest both show it").toBe(true);
		await h.command("review"); // Later
		expect(h.execs.filter(a => a[0] === "library"), "nothing approved or declined").toEqual([]);
		expect(h.statuses.at(-1), "still pending after the digest cursor moved").toBe("rematch: 1 waiting · /rematch review");
		await h.command("review"); // Inspect, then Decline
		expect(h.execs.filter(a => a[0] === "library")).toEqual([["library", "inspect", "hook-stuck"], ["library", "decline", "hook-stuck"]]);
		expect(h.titles.at(-1)).toContain("inspected hook-stuck");
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

	test("session_start carries identity and visible entries; session_shutdown the changed files", async () => {
		const top = fs.mkdtempSync(path.join(tmp, "top-"));
		const head = "a".repeat(40);
		const h = await host("identity", {
			entries: [{ id: "pkg", predicate_hit: true, version: 3, sha: "s3" }],
			hooks: [],
			visible: ["pkg", "kept"],
			git: { top, head, remote: "git@github.com:Org/Repo.git" },
		});
		h.ctx.cwd = path.join(top, "crates", "x");
		fs.mkdirSync(h.ctx.cwd, { recursive: true });
		await h.fire("session_start");
		const start = h.traceOf()[0];
		expect(start).toMatchObject({
			repo: top,
			repo_remote: "git@github.com:Org/Repo.git",
			subpath: "crates/x",
			head_sha: head,
			loaded_entries: [{ id: "pkg", version: 3, sha: "s3" }],
			visible_entries: ["pkg", "kept"],
		});
		fs.writeFileSync(path.join(top, "crates", "x", "lib.rs"), "pub fn x() {}");
		await h.fire("tool_result", { toolName: "write", toolCallId: "w1", input: { path: "lib.rs" }, content: [], isError: false });
		await h.fire("tool_result", { toolName: "write", toolCallId: "w2", input: { path: "failed.rs" }, content: [], isError: true });
		await h.fire("session_shutdown");
		const end = h.traceOf().at(-1) ?? {};
		expect(end.ev).toBe("session_shutdown");
		expect((end.files_changed as [string, string][]).map(([p]) => p)).toEqual(["crates/x/lib.rs"]);
	});

	test("without origin or a repo the identity fields are null and fail open", async () => {
		const top = fs.mkdtempSync(path.join(tmp, "unborn-"));
		const h = await host("no-origin", { entries: [], hooks: [], git: { top } });
		h.ctx.cwd = top;
		await h.fire("session_start");
		expect(h.traceOf()[0]).toMatchObject({ repo: top, repo_remote: null, subpath: "", head_sha: null, visible_entries: [] });
		await h.fire("session_shutdown");
		expect(h.traceOf().at(-1)?.files_changed).toBeUndefined();
	});

	test("each place mounts its own view, replacing the last: never a cumulative union", async () => {
		const root = path.join(tmp, "skills");
		const viewA = path.join(root, "views", "repo-a-0001", "skills");
		const viewB = path.join(root, "views", "repo-b-0002", "skills");
		const state: HostState = { entries: [], hooks: [], dirs: [viewA], view: "repo-a-0001" };
		const h = await host("skills", state);
		expect(h.execs.filter(a => a[0] === "skills")).toEqual([["skills", "sync", "--cwd", process.cwd(), "--harness", "omp"]]);
		expect(overridesOf(h.settings, "skills.customDirectories")).toEqual([["/configured/skills", viewA]]);
		// Another place: its view replaces this one (an older shared-dir entry the user had goes too).
		state.dirs = [viewB];
		state.view = "repo-b-0002";
		h.ctx.cwd = tmp;
		await h.fire("context", { messages: [user("q")] });
		expect(h.execs.filter(a => a[0] === "skills").map(a => a[3])).toEqual([process.cwd(), tmp]);
		expect(overridesOf(h.settings, "skills.customDirectories").at(-1)).toEqual(["/configured/skills", viewB]);
		expect(sessionSkillDirs(["/configured/skills", path.join(root, "skills"), viewA], [viewB], root)).toEqual(["/configured/skills", viewB]);
		await h.fire("session_start");
		expect(h.traceOf()[0]).toMatchObject({ staging_dirs: [viewB], exposure_view: "repo-b-0002" });
	});

	test("other places' provisional agents are disabled for the process; all of them when the binary cannot say", async () => {
		const h = await host("agents", { entries: [], hooks: [], withheld: ["other-repo-agent"] });
		expect(overridesOf(h.settings, "task.disabledAgents")).toEqual([["users-own-off", "other-repo-agent"]]);
		const root = path.join(tmp, "agents-failsafe");
		fs.mkdirSync(path.join(root, "agents"), { recursive: true });
		fs.writeFileSync(path.join(root, "agents", "a-trial.md"), "x");
		fs.writeFileSync(path.join(root, "agents", "b-trial.md"), "x");
		const failing = await host("agents-failsafe", { entries: [], hooks: [], syncFails: true });
		expect(overridesOf(failing.settings, "task.disabledAgents").at(-1)).toEqual(["users-own-off", "a-trial", "b-trial"]);
		expect(overridesOf(failing.settings, "skills.customDirectories").at(-1), "no rematch skill dir either").toEqual(["/configured/skills"]);
		// off from the command withdraws them at once
		await h.command("off");
		expect(overridesOf(h.settings, "task.disabledAgents").at(-1)).toEqual(["users-own-off"]);
	});

	test("a read credits the entry the session's view links, never a same-named skill of the user's", async () => {
		const root = path.join(tmp, "credit");
		const ours = path.join(root, "staging", "repo", "ledger");
		fs.mkdirSync(ours, { recursive: true });
		fs.writeFileSync(path.join(ours, "rematch.json"), JSON.stringify({ id: "e-ledger" }));
		const h = await host("credit", { entries: [], hooks: [], links: { ledger: ours, notes: path.join(root, "missing") } });
		await h.fire("session_start");
		await h.fire("tool_result", { toolName: "read", toolCallId: "r1", input: { path: "skill://ledger/units/a.md:1-5" }, content: [], isError: false });
		await h.fire("tool_result", { toolName: "read", toolCallId: "r2", input: { path: "skill://ledger" }, content: [], isError: true });
		await h.fire("tool_result", { toolName: "read", toolCallId: "r3", input: { path: "skill://notes" }, content: [], isError: false });
		const reads = h.traceOf().filter(r => r.ev === "tool_result").map(r => [r.skill_read, r.skill_entry]);
		expect(reads).toEqual([["ledger", "e-ledger"], ["ledger", null], ["notes", null]]);
		const mine = path.join(tmp, "my-skills");
		fs.mkdirSync(path.join(mine, "ledger"), { recursive: true });
		fs.writeFileSync(path.join(mine, "ledger", "SKILL.md"), "---\nname: ledger\ndescription: mine\n---\n");
		expect(shadowedByCustomDirs(new Map([["ledger", "e-ledger"]]), [mine], root), "omp gives the user's custom dir the bare name").toEqual(["ledger"]);
	});

	test("omp reads personal instructions from its AGENTS.md only: traced, never injected at runtime", async () => {
		const h = await host("instructions", {
			entries: [],
			hooks: [],
			instructions: { host: "omp", exposed: [{ id: "e1", name: "tests-first", sha: "f1", source: "ledger", bytes: 90 }], conflicts: [] },
			fragments: [{ id: "e1", name: "tests-first", text: "Run tests." }, { id: "e2", name: "legacy", text: "Legacy runtime text." }],
		});
		await h.fire("session_start");
		expect(h.traceOf()[0].instructions_exposed).toEqual([{ id: "e1", name: "tests-first", sha: "f1" }]);
		// A legacy runtime approval is not a second channel: it waits for the file approval.
		expect(await h.fire("before_agent_start", { systemPrompt: "base" })).toBeUndefined();
	});

	test("the model's rematch tool refuses every user-only control, wherever the global flags stand", async () => {
		const h = await host("tool-authority", { entries: [], hooks: [] });
		await h.fire("session_start");
		const refused: [string, string[]][] = [
			["library", ["approve", "e1"]],
			["library", ["--text", "approve", "e1"]],
			["library", ["--cwd", "/x", "decline", "e1"]],
			["library", ["--cwd=/x", "disable", "e1"]],
			["library", ["rollback", "ledger"]],
			["library", ["pin", "e1"]],
			["library", ["export", "--approve-all"]],
			["library", ["--text", "export", "--entry", "e1", "--approve"]],
			["off", []],
			["on", ["--text"]],
			["mark", ["correction"]],
			["uninstall", ["--yes"]],
		];
		for (const [op, args] of refused) {
			const r = await h.tool(op, args);
			expect(r.ok, `${op} ${args.join(" ")}`).toBe(false);
			expect(r.error, `${op} ${args.join(" ")}`).toContain("reserved to the user");
		}
		expect(h.execs.filter(a => ["library", "off", "on", "mark", "uninstall"].includes(a[0] ?? "") && a[1] !== "ls"), "none reached the binary").toEqual([]);
		expect((await h.tool("library", ["--text", "ls"])).ok).toBe(true);
		expect((await h.tool("library", ["inspect", "e1"])).ok).toBe(true);
		// The model's shell: the same parser, before any hook, and while rematch is off.
		expect(h.toolCall({ command: "rematch --text --cwd /w library disable e1" })).toContain("reserved to the user");
		expect(h.toolCall({ command: "cd /w && rematch --cwd=/w library export --approve-all" })).toContain("reserved to the user");
		expect(h.toolCall({ command: "rematch --text mark --class correction" })).toContain("reserved to the user");
		expect(h.toolCall({ command: "rematch policy scope set bellwether-dev ~/repos/bellwether" })).toContain("reserved to the user");
		expect(h.toolCall({ command: "rematch --cwd /w policy scope unset bellwether-dev" })).toContain("reserved to the user");
		expect(h.toolCall({ command: "rematch policy show" })).toBeUndefined();
		expect(h.toolCall({ command: "rematch --text library ls" })).toBeUndefined();
		expect(h.traceOf().filter(r => r.ev === "tool_blocked").map(r => r.hook_id)).toEqual(Array(5).fill("rematch:user-only"));
		await h.command("off");
		expect(h.toolCall({ command: "sh -c 'rematch --cwd /w on'" })).toContain("reserved to the user");
	});

	test("hook use is traced once per hook and tool, not on every call", async () => {
		const h = await host("hook-use", { entries: [], hooks: [stuckFile()] });
		await h.fire("session_start");
		h.toolCall(same);
		h.toolCall(same);
		expect(h.traceOf().filter(r => r.ev === "hook_evaluated")).toEqual([expect.objectContaining({ hook_ids: ["hook-stuck"], tool: "bash" })]);
	});

	test("session start runs one profile sync only when match reports a profile", async () => {
		const without = await host("no-profile", { entries: [], hooks: [] });
		await without.fire("session_start");
		expect(without.execs.filter(a => a[0] === "profile")).toEqual([]);
		const h = await host("profile", { entries: [], hooks: [], profilePresent: true });
		expect(h.execs.filter(a => a[0] === "profile")).toEqual([]);
		await h.fire("session_start");
		await h.fire("session_start");
		expect(h.execs.filter(a => a[0] === "profile")).toEqual([["profile", "sync", "--timeout", "8"]]);
	});

	describe("personal prompt fragments (helpers shared with Pi and OpenCode; omp reads its AGENTS.md)", () => {
		const fragments = [
			{ id: "a", name: "alpha", text: "\nUse alpha.\n\n" },
			{ id: "b", name: "beta", text: "Use beta." },
		];
		const block = "## rematch: alpha\n\nUse alpha.\n\n## rematch: beta\n\nUse beta.";

		test("omp never appends fragments to the system prompt, approved at runtime or not", async () => {
			const h = await host("fragments", { entries: [{ id: "a", predicate_hit: true }], hooks: [], fragments });
			await h.fire("session_start");
			expect(await h.fire("before_agent_start", { systemPrompt: ["base one", "base two"] })).toBeUndefined();
			expect(await h.fire("before_agent_start", { systemPrompt: "base" })).toBeUndefined();
		});
		test("helpers keep well-formed fragments, render the ship heading, and reject odd prompts", () => {
			expect(promptFragmentsOf({ prompt_fragments: [...fragments, { id: 1, name: "n", text: "t" }] })).toEqual(fragments);
			expect(promptFragmentsOf({})).toEqual([]);
			expect(renderPromptFragments([])).toBeNull();
			expect(renderPromptFragments(fragments)).toBe(block);
			expect(withPromptBlock(42, block)).toBeNull();
			expect(appendPromptFragments("base", fragments)).toBe(`base\n\n${block}`);
			expect(appendPromptFragments(["base"], fragments)).toEqual(["base", block]);
			expect(appendPromptFragments(["base"], [])).toBeNull();
		});
	});
});
