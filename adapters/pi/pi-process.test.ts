/**
 * Acceptance on real, stock Pi (`@earendil-works/pi-coding-agent`, the pinned devDependency) run
 * under Node with this package loaded by `-e`, the real `rematch` binary (`$REMATCH_BIN`, else the
 * repo's target/debug/rematch: `cargo build -p rematch` first) and a scripted OpenAI-compatible
 * chat-completions server registered through Pi's `models.json` — a local stand-in, no cloud
 * inference. HOME, Pi's agent dir and REMATCH_ROOT are temporary; nothing personal is read.
 *
 * - RPC: `resources_discover` puts the shared skill in the system prompt; the approved `retrieve`
 *   builtin adds its unit as a user-role custom message; input / tool_result (ok, error) / turns
 *   / settle are traced; the model cannot approve through the `rematch` tool nor through `bash`;
 *   `/rematch approve` re-binds the runtime (`/reload`) and the approved block hook rejects the
 *   next call in the same trace; a steer is a takeover; `/rematch mark`; shutdown → flagged
 *   pending file. `/rematch` output never reaches a provider request.
 * - Print + resume: `-p` traces and finishes; `count` counts it; `-c` resumes the counted
 *   session into a new segment `:r2`; `/rematch off` closes the segment, empties the shared dir
 *   and stops tracing; `/rematch on` re-binds and opens the next segment.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Server } from "bun";

const ADAPTER = import.meta.dir;
const REPO_ROOT = path.resolve(ADAPTER, "..", "..");
const PI_CLI = path.join(ADAPTER, "node_modules", "@earendil-works", "pi-coding-agent", "dist", "cli.js");
const BIN = process.env.REMATCH_BIN || path.join(REPO_ROOT, "target", "debug", "rematch");
const NODE = process.env.PI_NODE || "node";
const EDITOR_OK = path.join(REPO_ROOT, "crates", "rematch", "tests", "scripts", "editor-ok.sh");
const EDITOR_FRAGMENT = path.join(REPO_ROOT, "crates", "rematch", "tests", "scripts", "editor-fragment.sh");

if (!fs.existsSync(BIN)) throw new Error(`rematch binary missing at ${BIN}: cargo build -p rematch (or set REMATCH_BIN)`);
if (!fs.existsSync(PI_CLI)) throw new Error(`Pi missing at ${PI_CLI}: bun install in adapters/pi`);

type Json = Record<string, unknown>;
type Req = { messages: { role: string; content: unknown; tool_calls?: unknown }[]; tools?: { function: { name: string } }[] };
type Reply = { text?: string; tool?: { name: string; args: unknown } };

const text = (content: unknown): string =>
	typeof content === "string" ? content : Array.isArray(content) ? content.map((p: { text?: string }) => p.text ?? "").join("") : JSON.stringify(content ?? "");

/** Chat-completions SSE server answering from a queue of replies; records every request. */
class FakeModel {
	readonly requests: Req[] = [];
	readonly #server: Server<undefined>;
	#n = 0;
	constructor(readonly replies: Reply[]) {
		this.#server = Bun.serve({
			hostname: "127.0.0.1",
			port: 0,
			idleTimeout: 0,
			fetch: async req => {
				const body = (await req.json()) as Req;
				this.requests.push(body);
				const reply = this.replies.shift() ?? { text: "ok" };
				const id = `call_${++this.#n}`;
				const chunk = (delta: object, finish: string | null = null) =>
					`data: ${JSON.stringify({ id: "c", object: "chat.completion.chunk", created: 1, model: "fake-model", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
				let sse = chunk({ role: "assistant", content: "" });
				if (reply.text) sse += chunk({ content: reply.text });
				if (reply.tool) sse += chunk({ tool_calls: [{ index: 0, id, type: "function", function: { name: reply.tool.name, arguments: JSON.stringify(reply.tool.args) } }] });
				sse += chunk({}, reply.tool ? "tool_calls" : "stop");
				sse += `data: ${JSON.stringify({ id: "c", object: "chat.completion.chunk", created: 1, model: "fake-model", choices: [], usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 } })}\n\ndata: [DONE]\n\n`;
				return new Response(sse, { headers: { "content-type": "text/event-stream" } });
			},
		});
	}
	get baseUrl(): string {
		return `http://127.0.0.1:${this.#server.port}/v1`;
	}
	stop(): void {
		this.#server.stop(true);
	}
}

let t: string;
let home: string;
let agent: string;
let root: string;
let repo: string;
let model: FakeModel | undefined;

beforeEach(() => {
	t = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rematch-pi-proc-")));
	home = path.join(t, "home");
	agent = path.join(t, "pi-agent");
	root = path.join(t, "rematch");
	repo = path.join(t, "work", "payments");
	fs.mkdirSync(path.join(home, ".omp", "agent"), { recursive: true });
	fs.writeFileSync(path.join(home, ".omp", "agent", "config.yml"), "modelRoles:\n  smol: fakeprov/small-model\n  default: anthropic/claude-opus-5-5:high\n");
	fs.mkdirSync(agent, { recursive: true });
	fs.mkdirSync(path.join(repo, "src"), { recursive: true });
	fs.writeFileSync(path.join(repo, "src", "ledger.rs"), "pub fn reconcile() {}\n");
	const git = (...args: string[]) => spawnSync("git", ["-c", "user.email=t@t", "-c", "user.name=t", ...args], { cwd: repo });
	git("init", "-q");
	git("add", ".");
	git("commit", "-qm", "init");
});

afterEach(() => {
	model?.stop();
	model = undefined;
	fs.rmSync(t, { recursive: true, force: true });
});

/** The environment both rematch and Pi run with (no omp: no detached reflection). */
function env(): Record<string, string> {
	const e: Record<string, string> = {
		PATH: process.env.PATH ?? "/usr/bin:/bin",
		HOME: home,
		REMATCH_ROOT: root,
		REMATCH_BIN: BIN,
		REMATCH_MACHINE: "test-host",
		OMP_BIN: path.join(t, "no-omp"),
		PI_OFFLINE: "1",
		PI_TELEMETRY: "0",
		PI_SKIP_VERSION_CHECK: "1",
	};
	return e;
}

/** `rematch <args>` as a user's shell runs it (omp paths from HOME; Pi's dir is Pi's business). */
function cli(...args: string[]): Json {
	const r = spawnSync(BIN, args, { cwd: repo, env: env(), encoding: "utf8" });
	const v = JSON.parse(r.stdout.trim().split("\n").pop() ?? "{}") as Json;
	if (v.ok !== true) throw new Error(`rematch ${args.join(" ")}: ${r.stdout} ${r.stderr}`);
	return v.data as Json;
}

/** `rematch library approve` as the user's review runs it: the action's own argv (`--sha` of the reviewed snapshot). */
function approveAction(id: string): Json {
	const argv = (cli("library", "inspect", id).action as Json).commands as Json;
	return cli(...(argv.approve_argv as string[]));
}

function writeHookEditor(): string {
	const p = path.join(t, "hook-editor.sh");
	const reply = {
		decision: "EDIT",
		entry: {
			kind: "hook",
			name: "no-secrets-read",
			description: "Use when reading files: never read the secrets file.",
			trigger_line: "Use when reading project files.",
			files: { "SKILL.md": "---\nname: no-secrets-read\ndescription: Use when reading project files; never read secrets.txt.\n---\n\nUse when reading project files.\n\n## Laws\n\n- Never read `secrets.txt`; ask the user for the value instead.\n" },
			hook: { mode: "block", tool: "read", field: "path", regex: "(?:^|/)secrets\\.txt$", message: "rematch: secrets.txt is off limits; ask the user.", pass_condition: "Every read of a file not named secrets.txt passes." },
			stems: ["read", "secret"],
		},
		manifest: { evidence: { session_id: "", seq: 0 }, predicted_fix: "No secret reads.", predicted_regression: "None.", rationale: "The agent read secrets." },
	};
	fs.writeFileSync(p, `#!/bin/sh\ncat >/dev/null\ncat <<'J'\n\`\`\`json\n${JSON.stringify(reply)}\n\`\`\`\nJ\n`, { mode: 0o755 });
	return p;
}

/** A provisional skill package with a unit, the approved retrieve builtin, an unapproved block hook. */
function stage(): { skill: string; retrieve: string; hook: string } {
	const skill = cli("create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes", "--editor", EDITOR_OK).id as string;
	cli("activate", "ledger-test-gate", "--unverified");
	const unit = path.join(root, "staging", "payments", "ledger-test-gate", "units", "reconcile.md");
	fs.mkdirSync(path.dirname(unit), { recursive: true });
	fs.writeFileSync(unit, "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n");
	const builtins = (cli("builtins", "ls").builtins as Json[]).find(b => b.name === "context-unit-retrieval")!;
	const retrieve = builtins.id as string;
	approveAction(retrieve);
	const hook = cli("create", "no-secrets-read", "--from", "description", "--description", "never read the secrets file", "--editor", writeHookEditor()).id as string;
	cli("activate", "no-secrets-read", "--unverified");
	return { skill, retrieve, hook };
}

function writeModels(m: FakeModel): void {
	fs.writeFileSync(
		path.join(agent, "models.json"),
		JSON.stringify({ providers: { fakeai: { baseUrl: m.baseUrl, api: "openai-completions", apiKey: "test-key", models: [{ id: "fake-model", name: "Fake", contextWindow: 64000, maxTokens: 2000 }] } } }),
	);
}

const piArgs = (...rest: string[]) => [PI_CLI, "-e", ADAPTER, "--no-extensions", "--no-context-files", "--no-prompt-templates", "--no-themes", "--offline", "--model", "fakeai/fake-model", ...rest];
const piEnv = () => ({ ...env(), PI_CODING_AGENT_DIR: agent });

function traces(): string[] {
	const dir = path.join(root, "traces");
	return fs.existsSync(dir) ? fs.readdirSync(dir).filter(f => f.startsWith("pi:")).sort() : [];
}

function readTrace(name: string): Json[] {
	return fs.readFileSync(path.join(root, "traces", name), "utf8").trim().split("\n").map(l => JSON.parse(l) as Json);
}

/** `pi -p …` (async: the scripted model answers from this process's event loop). */
async function print(...args: string[]): Promise<{ code: number; stdout: string; stderr: string }> {
	const proc = Bun.spawn([NODE, ...piArgs(...args)], { cwd: repo, env: piEnv(), stdin: "ignore", stdout: "pipe", stderr: "pipe" });
	const [stdout, stderr, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
	return { code, stdout, stderr };
}

/** `pi --mode rpc`: send commands by id, await responses and settles; a clean close. */
async function rpc(...extra: string[]) {
	const proc = Bun.spawn([NODE, ...piArgs("--mode", "rpc", ...extra)], { cwd: repo, env: piEnv(), stdin: "pipe", stdout: "pipe", stderr: "pipe" });
	const stderr = new Response(proc.stderr).text();
	const records: Json[] = [];
	const waiters: { pred: (r: Json) => boolean; resolve: (r: Json) => void }[] = [];
	const reading = (async () => {
		const decoder = new TextDecoder();
		let buf = "";
		for await (const chunk of proc.stdout) {
			buf += decoder.decode(chunk, { stream: true });
			for (let nl = buf.indexOf("\n"); nl >= 0; nl = buf.indexOf("\n")) {
				const line = buf.slice(0, nl);
				buf = buf.slice(nl + 1);
				if (!line.trim()) continue;
				const r = JSON.parse(line) as Json;
				records.push(r);
				for (const w of [...waiters]) if (w.pred(r)) {
					waiters.splice(waiters.indexOf(w), 1);
					w.resolve(r);
				}
			}
		}
	})();
	const wait = (pred: (r: Json) => boolean, ms = 30_000): Promise<Json> => {
		const found = records.find(pred);
		if (found) return Promise.resolve(found);
		const { promise, resolve, reject } = Promise.withResolvers<Json>();
		waiters.push({ pred, resolve });
		const timer = setTimeout(() => reject(new Error(`rpc wait timed out; last records: ${JSON.stringify(records.slice(-5))}`)), ms);
		return promise.finally(() => clearTimeout(timer));
	};
	let n = 0;
	const send = async (cmd: Json): Promise<Json> => {
		const id = `r${++n}`;
		proc.stdin.write(`${JSON.stringify({ id, ...cmd })}\n`);
		await proc.stdin.flush();
		return wait(r => r.type === "response" && r.id === id);
	};
	let settles = 0;
	return {
		records,
		send,
		/** Prompt and wait for the settle of the run it starts. */
		async prompt(message: string): Promise<void> {
			const target = records.filter(r => r.type === "agent_settled").length + 1;
			const res = await send({ type: "prompt", message });
			expect(res).toMatchObject({ success: true, data: { disposition: "started" } });
			await wait(() => records.filter(r => r.type === "agent_settled").length >= target);
			settles = target;
		},
		/** An extension command: handled before the response. */
		async command(message: string): Promise<void> {
			const res = await send({ type: "prompt", message });
			expect(res).toMatchObject({ success: true, data: { disposition: "handled" } });
		},
		/** Answer an `extension_ui_request` dialog (`{ value }`, `{ confirmed }` or `{ cancelled: true }`). */
		async respond(id: unknown, answer: Json): Promise<void> {
			proc.stdin.write(`${JSON.stringify({ type: "extension_ui_response", id, ...answer })}\n`);
			await proc.stdin.flush();
		},
		wait,
		get settles() {
			return settles;
		},
		async close(): Promise<{ code: number; stderr: string }> {
			proc.stdin.end();
			const code = await proc.exited;
			await reading;
			return { code, stderr: await stderr };
		},
	};
}

const systemOf = (r: Req) => text(r.messages.find(m => m.role === "system" || m.role === "developer")?.content);

describe("stock Pi over RPC with a scripted model", () => {
	test("skills, retrieval, tracing, user-only approval, approve → reload → block, steer, mark, flagged shutdown", async () => {
		const { skill, retrieve, hook } = stage();
		const hookFile = path.join(home, ".omp", "agent", "extensions", "rematch-hooks", `${hook}.ts`);
		// The skill as Pi lists it: under this place's view (`skills sync --harness pi`), not the shared dir.
		const view = (cli("skills", "sync", "--harness", "pi", "--cwd", repo).dirs as string[])[0]!;
		const skillMd = path.join(view, "ledger-test-gate", "SKILL.md");
		expect(fs.existsSync(path.join(root, "skills", "ledger-test-gate")), "a trial is never in the shared dir").toBe(false);
		model = new FakeModel([
			{ tool: { name: "read", args: { path: skillMd } } },
			{ tool: { name: "bash", args: { command: "exit 3" } } },
			// The user's decisions, refused with the global flags first (clap accepts them anywhere).
			{ tool: { name: "rematch", args: { operation: "library", arguments: ["--text", "approve", hook] } } },
			{ tool: { name: "bash", args: { command: `rematch --cwd ${repo} --text library approve ${hook}` } } },
			{ text: "Committed after the ledger tests." },
			{ tool: { name: "read", args: { path: "secrets.txt" } } },
			{ tool: { name: "bash", args: { command: "sleep 3" } } },
			{ text: "Asked for the vault." },
		]);
		writeModels(model);
		const pi = await rpc();
		try {
			await pi.prompt("commit the ledger reconciliation migration change to the crate");
			expect(fs.existsSync(hookFile)).toBe(false); // neither the tool nor bash approved it
			// Typed without a hash: the action's dialog, whose Approve carries the reviewed snapshot.
			const approving = pi.command(`/rematch approve ${hook}`);
			const ask = await pi.wait(r => r.type === "extension_ui_request" && r.method === "select");
			expect(ask.options).toEqual(["Approve", "Decline", "Inspect", "Later"]);
			await pi.respond(ask.id, { value: "Approve" });
			await approving;
			expect(fs.existsSync(hookFile)).toBe(true);
			const second = pi.prompt("no, read the secrets file first");
			// Steer while `sleep 3` runs: Pi queues it, the adapter traces a takeover.
			await pi.wait(r => r.type === "tool_execution_start" && r.toolName === "bash" && JSON.stringify(r.args).includes("sleep"));
			const steer = await pi.send({ type: "prompt", message: "stop, use the vault instead", streamingBehavior: "steer" });
			expect(steer).toMatchObject({ success: true });
			await second;
			await pi.command("/rematch mark correction");
		} catch (e) {
			const out = await pi.close();
			throw new Error(`${e}\nstderr: ${out.stderr}`);
		}
		const out = await pi.close();
		expect(out.code).toBe(0);

		// One trace: the reviewed approval credited the newly loaded hook at once (one session_start
		// `pi:approve`), and its /reload continued the trace without re-opening it.
		const files = traces();
		expect(files).toHaveLength(1);
		const sid = files[0]!.replace(/\.jsonl$/, "");
		expect(sid).toMatch(/^pi:[0-9a-f-]{36}$/);
		const recs = readTrace(files[0]!);
		expect(recs.map(r => r.seq)).toEqual(recs.map((_, i) => i));
		const starts = recs.filter(r => r.ev === "session_start");
		expect(starts.map(r => r.mode)).toEqual(["pi:startup", "pi:approve"]);
		expect(starts[1]!.hooks_loaded).toEqual(expect.arrayContaining([retrieve, hook]));
		expect((starts[1]!.loaded_entries as Json[]).find(e => e.id === hook)).toMatchObject({ version: 1 });
		expect(recs.findIndex(r => r === starts[1])).toBeLessThan(recs.findIndex(r => r.ev === "tool_blocked" && r.hook_id === hook));
		expect(recs.filter(r => r.ev === "session_shutdown")).toHaveLength(1);
		const start = recs[0]!;
		expect(start).toMatchObject({ ev: "session_start", session_id: sid, cwd: repo, repo, mode: "pi:startup", agent: { kind: "main", name: "main" }, model: { provider: "fakeai", id: "fake-model" } });
		expect(String(start.omp_version)).toMatch(/^pi 1\.\d+\.\d+/);
		expect((start.loaded_entries as Json[]).map(e => e.id)).toContain(skill);
		expect((start.loaded_entries as Json[]).find(e => e.id === skill)).toMatchObject({ version: 1 });
		expect((start.loaded_entries as Json[]).map(e => e.id)).not.toContain(retrieve);
		expect(start.hooks_loaded).toEqual([retrieve]);
		// This place's view (kept library + its provisional entries), never the shared dir of every repo's trials.
		expect((start.staging_dirs as string[]).map(d => path.relative(root, d).split(path.sep).slice(0, 1).concat(path.basename(d)))).toEqual([["views", "skills"]]);
		expect(String(start.exposure_view)).toMatch(/^payments-[0-9a-f]{16}$/);
		expect(start.visible_entries).toContain(skill);

		const evs = recs.map(r => (r.ev === "tool_result" || r.ev === "tool_blocked" ? `${r.ev}:${r.tool}` : r.ev));
		expect(evs[1]).toBe("input");
		expect(recs[1]).toMatchObject({ source: "rpc", idle: true, prelabel: "none", text: "commit the ledger reconciliation migration change to the crate" });
		const retrieval = recs.find(r => r.ev === "retrieval")!;
		expect(retrieval).toMatchObject({ entry_ids: [retrieve], units: [{ entry_id: skill, path: "units/reconcile.md" }] });
		const results = recs.filter(r => r.ev === "tool_result");
		expect(results.slice(0, 1).map(r => [r.tool, r.is_error, r.skill_read])).toEqual([
			["read", false, "ledger-test-gate"],
		]);
		expect(results[0]!.skill_entry, "the read resolves to the trial's entry").toBe(skill);
		expect(results.slice(1).map(r => [r.tool, r.is_error, r.skill_read])).toEqual([
			["bash", true, null],
			["rematch", true, null],
			["bash", false, null],
		]);
		expect(String(results[2]!.content_head)).toContain("reserved to the user");
		const blocked = recs.filter(r => r.ev === "tool_blocked");
		expect(blocked.map(r => [r.tool, r.hook_id])).toEqual([
			["bash", "rematch:user-only"],
			["read", hook],
		]);
		const inputs = recs.filter(r => r.ev === "input");
		expect(inputs.map(r => [r.prelabel, r.idle, r.source])).toEqual([
			["none", true, "rpc"],
			["negation", true, "rpc"],
			["interrupt", false, "rpc"],
		]);
		expect(inputs[1]!.after_deliverable).toBe("Committed after the ledger tests.");
		const turns = recs.filter(r => r.ev === "turn_end");
		expect(turns[0]).toMatchObject({ model: { provider: "fakeai", id: "fake-model" }, stop_reason: "toolUse" });
		expect((turns[0]!.usage as Json).input).toBeGreaterThan(0);
		expect(recs.filter(r => r.ev === "agent_end").map(r => r.yielded)).toEqual([true, true]);
		expect(recs.find(r => r.ev === "mark")).toMatchObject({ class: "correction" });
		const shut = recs.at(-1)!;
		expect(shut).toMatchObject({ ev: "session_shutdown", flagged: true });
		expect(shut.reasons).toEqual(["redirect:negation", "redirect:interrupt", "mark:correction"]);
		const pending = JSON.parse(fs.readFileSync(path.join(root, "pending", `${sid}.json`), "utf8")) as Json;
		expect(pending).toMatchObject({ session_id: sid, source: "pi", reasons: shut.reasons });
		// Counted once, with the approved blocking hook among the session's loaded entries.
		expect(cli("count").folded as string[]).toContain(sid);
		expect(cli("count").folded as string[]).not.toContain(sid);
		const why = cli("library", "why", sid);
		expect((why.loaded as Json[]).map(e => e.id)).toEqual(expect.arrayContaining([skill, hook]));
		const counters = ((cli("library", "ls").entries as Json[]).find(e => e.id === hook)!.counters as Json);
		expect(counters.matched).toBe(1);

		// What the model saw.
		const first = model.requests[0]!;
		expect(systemOf(first)).toContain("<name>ledger-test-gate</name>");
		expect(systemOf(first)).toContain(`<location>${skillMd}</location>`);
		expect(systemOf(first)).not.toContain("no-secrets-read"); // hooks reach Pi as code, their router only once approved
		const injected = first.messages.filter(m => text(m.content).startsWith("[rematch] Units"));
		expect(injected.map(m => m.role)).toEqual(["user"]);
		expect(text(injected[0]!.content)).toContain('<unit skill="ledger-test-gate" path="units/reconcile.md">');
		expect((first.tools ?? []).map(x => x.function.name)).toContain("rematch");
		const afterBlock = model.requests[6]!;
		expect(afterBlock.messages.map(m => text(m.content)).join("\n")).toContain("secrets.txt is off limits");
		const everything = model.requests.map(r => JSON.stringify(r)).join("\n");
		expect(everything).not.toContain("approved hook of no-secrets-read"); // /rematch output stays out of the provider context
		expect(model.requests.at(-1)!.messages.map(m => text(m.content)).join("\n")).toContain("stop, use the vault instead");
	}, 120_000);
});

describe("stock Pi, approved personal instructions", () => {
	test("the approved fragment reaches Pi's system prompt and is traced as sent, bound to its approved hash, once", async () => {
		const id = cli("create", "crate-tests-first", "--from", "description", "--description", "run crate tests before committing", "--editor", EDITOR_FRAGMENT).id as string;
		cli("activate", "crate-tests-first", "--unverified");
		const fragmentFile = path.join(root, "staging", "payments", "crate-tests-first", "AGENTS.fragment.md");
		const sha = createHash("sha256").update(fs.readFileSync(fragmentFile)).digest("hex");
		model = new FakeModel([{ text: "Not yet." }, { text: "Tests first." }, { text: "Again." }]);
		writeModels(model);
		expect((await print("-p", "commit the crate change")).code).toBe(0);
		expect(systemOf(model.requests[0]!)).not.toContain("## rematch: crate-tests-first"); // not approved
		approveAction(`instructions:${id}`);

		const pi = await rpc();
		try {
			await pi.prompt("commit the crate change");
			await pi.prompt("and the next crate");
		} catch (e) {
			const out = await pi.close();
			throw new Error(`${e}\nstderr: ${out.stderr}`);
		}
		expect((await pi.close()).code).toBe(0);
		const block = `## rematch: crate-tests-first\n\n${fs.readFileSync(fragmentFile, "utf8").trim()}`;
		expect(systemOf(model.requests[1]!)).toContain(block);
		expect(systemOf(model.requests[2]!)).toContain(block);
		const recs = traces().flatMap(readTrace);
		const sent = recs.filter(r => r.ev === "fragments_sent");
		expect(sent).toHaveLength(1);
		expect(sent[0]).toMatchObject({ host: "pi", fragments: [{ id, name: "crate-tests-first", sha, via: "pi:before_agent_start" }], bytes: Buffer.byteLength(block, "utf8") });
	}, 120_000);
});

describe("stock Pi, print mode, resume and off", () => {
	test("-p traces; a counted session resumed with -c continues in :r2; off closes and stops; on re-binds into the next segment", async () => {
		const { skill } = stage();
		model = new FakeModel([{ text: "Hello." }, { text: "Continued." }, { text: "Off reply." }, { text: "On reply." }]);
		writeModels(model);

		const p1 = await print("-p", "hello there, reconcile the ledger");
		expect(p1.code).toBe(0);
		expect(p1.stdout).toContain("Hello.");
		const [first] = traces();
		const sid = first!.replace(/\.jsonl$/, "");
		const r1 = readTrace(first!);
		expect(r1.map(r => r.ev)).toEqual(["session_start", "input", "turn_end", "agent_end", "session_shutdown"]);
		expect(r1[1]).toMatchObject({ idle: true, prelabel: "none" });
		const printSource = r1[1]!.source;
		const counted = cli("count");
		expect(counted.folded as string[]).toContain(sid);
		const before = fs.readFileSync(path.join(root, "traces", first!), "utf8");

		const p2 = await print("-c", "-p", "continue the work");
		expect(p2.code).toBe(0);
		expect(traces()).toEqual([first!, `${sid}:r2.jsonl`]);
		expect(fs.readFileSync(path.join(root, "traces", first!), "utf8")).toBe(before); // the counted segment is never finalized again
		const r2 = readTrace(`${sid}:r2.jsonl`);
		expect(r2.map(r => r.ev)).toEqual(["session_start", "input", "turn_end", "agent_end", "session_shutdown"]);
		expect(r2[0]).toMatchObject({ session_id: `${sid}:r2` });
		expect((r2[0]!.loaded_entries as Json[]).map(e => e.id)).toContain(skill);
		expect(r2[1]).toMatchObject({ text: "continue the work", after_deliverable: "Hello." });
		expect(cli("count").folded as string[]).toEqual([`${sid}:r2`]);

		// Off and on inside one resumed RPC session.
		const pi = await rpc("-c");
		try {
			await pi.command("/rematch off");
			const r3 = readTrace(`${sid}:r3.jsonl`);
			expect(r3.map(r => r.ev)).toEqual(["session_start", "session_shutdown"]);
			expect(fs.readdirSync(path.join(root, "skills")).filter(n => !n.startsWith("."))).toEqual([]);
			await pi.prompt("anything while off");
			expect(systemOf(model.requests.at(-1)!)).not.toContain("ledger-test-gate");
			expect(readTrace(`${sid}:r3.jsonl`)).toHaveLength(2);
			await pi.command("/rematch on");
			await pi.prompt("back on, reconcile the ledger");
			expect(systemOf(model.requests.at(-1)!)).toContain("<name>ledger-test-gate</name>");
		} catch (e) {
			const out = await pi.close();
			throw new Error(`${e}\nstderr: ${out.stderr}`);
		}
		expect((await pi.close()).code).toBe(0);
		expect(traces()).toEqual([first!, `${sid}:r2.jsonl`, `${sid}:r3.jsonl`, `${sid}:r4.jsonl`]);
		const r4 = readTrace(`${sid}:r4.jsonl`);
		expect(r4.map(r => r.ev)).toEqual(["session_start", "input", "turn_end", "agent_end", "session_shutdown"]);
		expect(r4[0]).toMatchObject({ mode: "pi:reload" });
		expect(r4[1]).toMatchObject({ source: "rpc", text: "back on, reconcile the ledger" });
		console.log(`observed print-mode input source: ${String(printSource)}`);
	}, 120_000);
});

describe("pi install <checkout>/adapters/pi", () => {
	test("installs in place, loads without -e, and doctor finds the package", async () => {
		const install = Bun.spawn([NODE, PI_CLI, "install", ADAPTER], { cwd: repo, env: piEnv(), stdout: "pipe", stderr: "pipe" });
		const [installOut, installCode] = await Promise.all([new Response(install.stdout).text(), install.exited]);
		expect(installCode).toBe(0);
		expect(installOut).toContain(`Installed ${ADAPTER}`);
		const settings = JSON.parse(fs.readFileSync(path.join(agent, "settings.json"), "utf8")) as { packages: string[] };
		expect(settings.packages).toHaveLength(1);
		expect(path.resolve(agent, settings.packages[0]!)).toBe(ADAPTER); // a local source is loaded from the checkout, not copied

		const run = Bun.spawn([NODE, PI_CLI, "--offline", "-p", "/rematch doctor"], { cwd: repo, env: piEnv(), stdout: "pipe", stderr: "pipe" });
		const [out, err, code] = await Promise.all([new Response(run.stdout).text(), new Response(run.stderr).text(), run.exited]);
		expect(code).toBe(0);
		const report = `${out}\n${err}`;
		expect(report).toContain(`pi: ok · package ${ADAPTER}`);
		expect(report).toContain(`agent_dir: ${path.join(home, ".omp", "agent")}`); // Pi's PI_CODING_AGENT_DIR never became omp's
	}, 60_000);
});

describe("detached reflection", () => {
	test("session start runs `rematch reflect --pending` detached only where omp exists", async () => {
		const log = path.join(t, "argv.log");
		const wrapper = path.join(t, "rematch-logged");
		fs.writeFileSync(wrapper, `#!/bin/sh\necho "$*" >> '${log}'\nexec '${BIN}' "$@"\n`, { mode: 0o755 });
		const fakeOmp = path.join(t, "omp");
		fs.writeFileSync(fakeOmp, "#!/bin/sh\nexit 1\n", { mode: 0o755 });
		const once = async (omp: string) => {
			const run = Bun.spawn([NODE, PI_CLI, "-e", ADAPTER, "--no-extensions", "--offline", "-p", "/rematch status"], { cwd: repo, env: { ...piEnv(), REMATCH_BIN: wrapper, OMP_BIN: omp }, stdout: "pipe", stderr: "pipe" });
			expect(await run.exited).toBe(0);
		};
		await once(path.join(t, "no-omp"));
		expect(fs.readFileSync(log, "utf8")).not.toContain("reflect --pending");
		await once(fakeOmp);
		// The detached child outlives `pi -p`; its argv line is its only trace (bounded wait, no event to await).
		const deadline = Date.now() + 10_000;
		while (!fs.readFileSync(log, "utf8").includes("reflect --pending") && Date.now() < deadline) await Bun.sleep(50);
		expect(fs.readFileSync(log, "utf8")).toContain("reflect --pending");
	}, 60_000);
});

describe("abort is not a takeover", () => {
	test("an Esc-aborted run, then an ordinary prompt: idle, prelabel none; the abort is the run's unyielded agent_end", async () => {
		model = new FakeModel([{ tool: { name: "bash", args: { command: "sleep 5" } } }, { text: "Added." }]);
		writeModels(model);
		const pi = await rpc();
		try {
			const first = pi.prompt("add the reconciliation command");
			await pi.wait(r => r.type === "tool_execution_start" && r.toolName === "bash");
			expect(await pi.send({ type: "abort" })).toMatchObject({ success: true });
			await first;
			await pi.prompt("now write the changelog entry");
		} catch (e) {
			const out = await pi.close();
			throw new Error(`${e}\nstderr: ${out.stderr}`);
		}
		expect((await pi.close()).code).toBe(0);
		const recs = readTrace(traces()[0]!);
		expect(recs.filter(r => r.ev === "input").map(r => [r.prelabel, r.idle])).toEqual([
			["none", true],
			["none", true],
		]);
		expect(recs.filter(r => r.ev === "agent_end").map(r => r.yielded)).toEqual([false, true]);
		expect(recs.at(-1)).toMatchObject({ ev: "session_shutdown", flagged: false, reasons: [] });
		// The history import of the same session file agrees (its live trace moved aside).
		// Pi writes the abort as a `Command aborted` error result and an empty `error` assistant message.
		const sessionFile = fs.readdirSync(path.join(agent, "sessions"), { recursive: true }).map(String).find(f => f.endsWith(".jsonl"))!;
		expect(fs.readFileSync(path.join(agent, "sessions", sessionFile), "utf8")).toContain('"text":"Command aborted"');
		fs.renameSync(path.join(root, "traces", traces()[0]!), path.join(t, "live.jsonl"));
		const imported = cli("import", "--history", "--harness", "pi", "--sessions", path.join(agent, "sessions"));
		expect((imported.history_pi as Json).redirects).toBe(0);
	}, 60_000);
});
