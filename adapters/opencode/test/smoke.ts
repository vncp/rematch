// Real-runtime acceptance for the OpenCode adapter: the pinned `opencode-ai` binary (this
// package's devDependency) runs with the plugin from this checkout in an isolated HOME/XDG,
// REMATCH_ROOT and project, against a scripted OpenAI-compatible provider served here. No
// cloud inference: every model reply is fixed by SCRIPT below. Run from adapters/opencode
// after `npm install` and `cargo build -p rematch`: `bun test/smoke.ts` (exit 0 = all checks).
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { Database } from "bun:sqlite";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const here = path.dirname(new URL(import.meta.url).pathname);
const adapter = path.resolve(here, "..");
const repoRoot = path.resolve(adapter, "../..");
const opencodeBin = path.join(adapter, "node_modules/.bin/opencode");
const rematchBin = process.env.REMATCH_BIN ?? path.join(repoRoot, "target/debug/rematch");
const editorSkill = path.join(repoRoot, "crates/rematch/tests/scripts/editor-ok.sh");
const editorHook = path.join(here, "editor-block-hook.sh");
const editorFragment = path.join(repoRoot, "crates/rematch/tests/scripts/editor-fragment.sh");

const T = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rematch-oc-smoke-")));
const home = path.join(T, "home");
const xdg = {
	XDG_CONFIG_HOME: path.join(home, ".config"),
	XDG_DATA_HOME: path.join(home, ".local/share"),
	XDG_CACHE_HOME: path.join(home, ".cache"),
	XDG_STATE_HOME: path.join(home, ".local/state"),
};
const root = path.join(T, "rematch");
const proj = path.join(T, "work/payments");
for (const d of [home, ...Object.values(xdg), path.join(T, "tmp"), proj]) fs.mkdirSync(d, { recursive: true });

// No omp on PATH: the plugin must not start a reflection that would call a real model.
const basePath = ["/usr/bin", "/bin", "/usr/sbin", "/sbin", path.dirname(process.execPath)].join(":");
const env: Record<string, string> = {
	PATH: basePath,
	HOME: home,
	...xdg,
	TMPDIR: path.join(T, "tmp"),
	REMATCH_ROOT: root,
	REMATCH_BIN: rematchBin,
	REMATCH_MACHINE: "smoke-host",
	OPENCODE_DISABLE_MODELS_FETCH: "1",
	OPENCODE_DISABLE_AUTOUPDATE: "1",
	OPENCODE_DISABLE_TERMINAL_TITLE: "1",
};

/**
 * Parsed JSON (rematch envelopes, trace lines, provider requests) as the checks read it: each
 * check states the shape it expects and fails, with the value, when it is not there.
 */
// biome-ignore lint/suspicious/noExplicitAny: the smoke test reads arbitrary JSON and asserts on it.
type Loose = Record<string, any>;

let failures = 0;
function check(ok: boolean, what: string, detail?: unknown) {
	console.log(`${ok ? "ok  " : "FAIL"} ${what}${ok || detail === undefined ? "" : `\n     ${JSON.stringify(detail).slice(0, 1500)}`}`);
	if (!ok) failures++;
}

function rematch(args: string[], extra: Record<string, string> = {}) {
	const r = spawnSync(rematchBin, args, { cwd: proj, env: { ...env, ...extra }, encoding: "utf8" });
	const line = r.stdout.trim().split("\n").pop() ?? "";
	try {
		return JSON.parse(line);
	} catch {
		throw new Error(`rematch ${args.join(" ")}: ${r.stdout} ${r.stderr}`);
	}
}

/** `rematch library approve` as the user's review runs it: the action's own argv (`--sha` of the reviewed snapshot). */
function approveAction(id: string) {
	const inspected = rematch(["library", "inspect", id]);
	const argv = inspected?.data?.action?.commands?.approve_argv;
	if (!Array.isArray(argv)) throw new Error(`${id} has no approve_argv: ${JSON.stringify(inspected)}`);
	return rematch(argv);
}

const trace = (sid: string): Loose[] => {
	const p = path.join(root, "traces", `${sid}.jsonl`);
	return fs.existsSync(p) ? fs.readFileSync(p, "utf8").trim().split("\n").filter(Boolean).map(l => JSON.parse(l)) : [];
};
const evs = (recs: Loose[]) => recs.map(r => r.ev);

// --- rematch: a provisional skill, an approved block hook, the built-ins (unapproved) ---------
spawnSync("git", ["init", "-q"], { cwd: proj, env });
fs.writeFileSync(path.join(proj, "README.md"), "payments\n");
const skill = rematch(["create", "run-tests-before-commit", "--from", "description", "--description", "Run the crate tests before committing", "--editor", editorSkill]);
rematch(["activate", "run-tests-before-commit", "--unverified"]);
const hook = rematch(["create", "no-recursive-delete", "--from", "description", "--description", "Never rm -rf in the shell", "--editor", editorHook]);
rematch(["activate", "no-recursive-delete", "--unverified"]);
const approved = approveAction(hook.data.id);
check(approved.ok, "block hook approved by the user (CLI)", approved);
const builtins = rematch(["builtins", "ls"]).data.builtins as Loose[];
const stuck = builtins.find(b => b.name === "stuck-detector")?.id as string;
// An approved `retrieve` hook, and a unit in the skill package that matches the first prompt.
const retrieveHook = builtins.find(b => b.name === "context-unit-retrieval")?.id as string;
approveAction(retrieveHook);
const unit = path.join(root, "staging/payments/run-tests-before-commit/units/ledger-migration.md");
fs.mkdirSync(path.dirname(unit), { recursive: true });
fs.writeFileSync(unit, "# Ledger migration\n\nRun the ledger migration tests before you commit the crate changes.\n");
/** System prompts the scripted provider received (the `retrieve` units arrive there). */
const systems: string[] = [];

// --- the scripted provider (OpenAI chat completions, streamed) --------------------------------
const SCRIPT: Record<string, Record<string, unknown>[]> = {
	"migrate the ledger": [
		{ tool: "skill", args: { name: "run-tests-before-commit" } },
		{ tool: "bash", args: { command: "echo migrated", description: "migrate" } },
		{ tool: "bash", args: { command: "ls /definitely/missing/dir", description: "inspect" } },
		{ tool: "bash", args: { command: "rm -rf build", description: "clean" } },
		{ tool: "bash", args: { command: `${rematchBin} --text --cwd ${proj} library approve ${stuck}`, description: "self-approve, global flags first" } },
		{ tool: "read", args: { filePath: path.join(proj, "missing.txt") } },
		{ tool: "rematch", args: { operation: "status", arguments: [] } },
		{ tool: "rematch", args: { operation: "approve", arguments: [stuck] } },
		{ text: "Migration done." },
	],
	"no, that is wrong": [{ text: "Understood, reverting the approach." }],
	"first served prompt": [{ tool: "bash", args: { command: "echo one", description: "one" } }, { text: "first answer" }],
	"second served prompt": [{ text: "second answer" }],
	"unrelated later prompt": [{ text: "later answer" }],
	"fragment carrying prompt": [{ text: "tests first, noted" }],
};
const textOf = (c: unknown): string =>
	typeof c === "string" ? c : Array.isArray(c) ? c.map(x => (typeof x === "string" ? x : (x?.text ?? ""))).join("") : "";
const server = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	async fetch(req) {
		const body = (await req.json().catch(() => ({}))) as Loose;
		const msgs: Loose[] = body.messages ?? [];
		for (const m of msgs) if (m.role === "system") systems.push(textOf(m.content));
		let step: Record<string, unknown> = { text: "Scripted title" };
		if (Array.isArray(body.tools) && body.tools.length > 0) {
			let last = -1;
			for (let i = msgs.length - 1; i >= 0; i--) if (msgs[i].role === "user") {
				last = i;
				break;
			}
			const user = last >= 0 ? textOf(msgs[last].content) : "";
			const after = msgs.slice(last + 1).filter(m => m.role === "assistant").length;
			const key = Object.keys(SCRIPT).find(k => user.includes(k));
			const steps = key ? SCRIPT[key] : [{ text: "no script" }];
			step = steps[Math.min(after, steps.length - 1)];
		}
		const id = `chatcmpl-${Math.random().toString(36).slice(2)}`;
		const chunk = (delta: unknown, finish: string | null) =>
			`data: ${JSON.stringify({ id, object: "chat.completion.chunk", created: 0, model: body.model, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
		let out = "";
		if (step.tool) {
			out += chunk({ role: "assistant", tool_calls: [{ index: 0, id: `call_${Math.random().toString(36).slice(2, 10)}`, type: "function", function: { name: step.tool, arguments: JSON.stringify(step.args ?? {}) } }] }, null);
			out += chunk({}, "tool_calls");
		} else {
			out += chunk({ role: "assistant", content: step.text }, null);
			out += chunk({}, "stop");
		}
		out += `data: ${JSON.stringify({ id, object: "chat.completion.chunk", created: 0, model: body.model, choices: [], usage: { prompt_tokens: 11, completion_tokens: 7, total_tokens: 18 } })}\n\n`;
		out += "data: [DONE]\n\n";
		return new Response(out, { headers: { "content-type": "text/event-stream" } });
	},
});

fs.mkdirSync(path.join(xdg.XDG_CONFIG_HOME, "opencode"), { recursive: true });
fs.writeFileSync(
	path.join(xdg.XDG_CONFIG_HOME, "opencode/opencode.json"),
	JSON.stringify(
		{
			$schema: "https://opencode.ai/config.json",
			plugin: [adapter],
			model: "mock/scripted",
			small_model: "mock/scripted",
			provider: {
				mock: {
					npm: "@ai-sdk/openai-compatible",
					name: "Scripted",
					options: { baseURL: `http://127.0.0.1:${server.port}/v1`, apiKey: "none" },
					models: { scripted: { name: "scripted", tool_call: true, limit: { context: 100000, output: 4000 } } },
				},
			},
			permission: { bash: "allow", read: "allow", skill: "allow", external_directory: "allow" },
		},
		null,
		1,
	),
);
/** `opencode …` as a child process; asynchronous, so the scripted provider in this process keeps serving. */
async function run(args: string[]) {
	const child = spawn(opencodeBin, args, { cwd: proj, env, stdio: ["ignore", "pipe", "pipe"] });
	let err = "";
	child.stderr?.on("data", (d: Buffer) => (err += d.toString()));
	child.stdout?.resume();
	const timer = setTimeout(() => child.kill("SIGKILL"), 120_000);
	const code = await new Promise<number | null>(resolve => child.on("close", resolve));
	clearTimeout(timer);
	if (code !== 0) console.log(`opencode ${args.join(" ")} exited ${code}\n${err.slice(-2000)}`);
}
const store = () => new Database(path.join(xdg.XDG_DATA_HOME, "opencode/opencode.db"), { readonly: true });
const sessionIds = (): string[] => (store().query("SELECT id FROM session WHERE parent_id IS NULL ORDER BY time_created").all() as { id: string }[]).map(r => r.id);

// --- A: one `opencode run`: shared skill, tools, failures, an approved block, a refused self-approval
await run(["run", "migrate the ledger now and commit the crate changes"]);
const [s1] = sessionIds();
const a = trace(`oc:${s1}`);
check(a.length > 0, "plugin loaded: trace oc:<session> written", { s1, traces: fs.readdirSync(path.join(root, "traces")) });
const start = a[0] ?? {};
check(start.ev === "session_start" && start.mode === "opencode" && /^opencode\b/.test(start.omp_version), "session_start from the plugin", start);
check((start.loaded_entries ?? []).some((e: Loose) => e.id === skill.data.id && e.version >= 1 && e.sha), "skill entry loaded with version and sha", start.loaded_entries);
check((start.hooks_loaded ?? []).includes(hook.data.id), "approved block hook loaded", start.hooks_loaded);
check(!(start.hooks_loaded ?? []).includes(stuck), "unapproved built-in not loaded", start.hooks_loaded);
check((start.visible_entries ?? []).includes(skill.data.id), "skill visible through this directory's view", start.visible_entries);
check(/^payments-[0-9a-f]{16}$/.test(String(start.exposure_view)) && (start.staging_dirs ?? []).every((d: string) => d.startsWith(path.join(root, "views"))), "OpenCode was given this directory's view, not the shared dir", start);
check(!fs.existsSync(path.join(root, "skills", "run-tests-before-commit")), "the provisional skill is not in the shared dir");
const tools = a.filter(r => r.ev === "tool_result");
const skillCall = tools.find(r => r.tool === "skill");
check(skillCall?.skill_read === "run-tests-before-commit" && !skillCall.is_error, "shared skill read through OpenCode's native skill tool", skillCall);
check(skillCall?.skill_entry === skill.data.id, "the skill tool's load credits the entry this directory's view links", skillCall);
check(String(skillCall?.content_head).includes("run-tests-before-commit"), "skill content came from the shared dir", skillCall?.content_head);
check(tools.some(r => r.tool === "bash" && r.input.command === "echo migrated" && !r.is_error), "successful bash traced", tools);
check(tools.some(r => r.tool === "bash" && r.input.command.startsWith("ls /definitely") && r.is_error), "non-zero exit traced as an error", tools);
check(tools.some(r => r.tool === "read" && r.is_error && /not found/i.test(r.content_head)), "failed read (error part) traced", tools);
const blocked = a.find(r => r.ev === "tool_blocked");
check(blocked?.hook_id === hook.data.id && blocked?.tool === "bash" && blocked?.args_hash?.length === 64, "approved block hook refused rm -rf (tool_blocked)", blocked);
const rmPart = tools.find(r => r.input?.command === "rm -rf build");
check(rmPart?.is_error === true && String(rmPart.content_head).includes("rm -rf is blocked"), "the block reason became the tool error", rmPart);
const selfApprove = tools.find(r => String(r.input?.command).includes("library approve"));
check(selfApprove?.is_error === true && String(selfApprove.content_head).includes("reserved to the user"), "model's self-approval refused, global flags first", selfApprove);
const hooksDir = path.join(home, ".omp/agent/extensions/rematch-hooks");
check(fs.existsSync(path.join(hooksDir, `${hook.data.id}.ts`)) && !fs.existsSync(path.join(hooksDir, `${stuck}.ts`)), "stuck-detector still unapproved (no compiled module)");
const status = tools.find(r => r.tool === "rematch" && r.input?.operation === "status");
check(status?.is_error === false && /"(?:entries|actions)"/.test(String(status.content_head)), "read-only rematch tool answered", status);
const toolApprove = tools.find(r => r.tool === "rematch" && r.input?.operation === "approve");
check(String(toolApprove?.content_head).startsWith("unknown operation"), "rematch tool has no approve operation", toolApprove);
const turns = a.filter(r => r.ev === "turn_end");
check(turns.length === 9 && turns.every(t => t.model?.provider === "mock" && t.model?.id === "scripted" && t.usage?.input === 11), "one turn_end per model call, model and usage", turns);
const retrieval = a.find(r => r.ev === "retrieval");
check(
	retrieval?.entry_ids?.[0] === retrieveHook && retrieval.units.some((u: Loose) => u.entry_id === skill.data.id && u.path === "units/ledger-migration.md"),
	"retrieve: units attributed to the hook and the package",
	retrieval,
);
check(a.filter(r => r.ev === "retrieval").length === 1, "one retrieval record while the unit set holds", evs(a));
check((start.hooks_loaded ?? []).includes(retrieveHook) && !(start.loaded_entries ?? []).some((e: Loose) => e.id === retrieveHook), "retrieve hook credits through retrieval, not loaded_entries", start);
check(systems.filter(s => s.includes("[rematch] Units of skills loaded in this session")).length >= 9 && systems.some(s => s.includes("Run the ledger migration tests")), "unit text reached each model request's system prompt", systems.length);
check(a.filter(r => r.ev === "input").length === 1, "retrieved units are not a user input", a.filter(r => r.ev === "input"));
check(a.filter(r => r.ev === "input").length === 1 && a.find(r => r.ev === "input")?.text.includes("migrate the ledger"), "one input (the typed prompt)", a.filter(r => r.ev === "input"));
const shut = a.at(-1);
check(shut?.ev === "session_shutdown" && shut.flagged === true, "dispose finalized the segment, flagged (error cluster)", shut);
check(fs.existsSync(path.join(root, "pending", `oc:${s1}.json`)), "pending reflection file written");
check(JSON.parse(fs.readFileSync(path.join(root, "pending", `oc:${s1}.json`), "utf8")).source === "opencode", "pending file source opencode");
check(!fs.existsSync(path.join(root, "opencode/open", `oc:${s1}.json`)), "owner file removed");

// --- B: the same session continued by another process: a new segment, nothing appended ------
const before = fs.readFileSync(path.join(root, "traces", `oc:${s1}.jsonl`), "utf8");
// Separate words: `opencode run` wraps an argument holding a space in literal quotes.
await run(["run", "--session", s1, "no,", "that", "is", "wrong"]);
check(fs.readFileSync(path.join(root, "traces", `oc:${s1}.jsonl`), "utf8") === before, "finished segment never appended to");
const r2 = trace(`oc:${s1}:r2`);
check(r2[0]?.mode === "opencode:resume" && r2[0]?.session_id === `oc:${s1}:r2`, "resume opened segment r2", r2[0]);
check(evs(r2).join(",") === "session_start,input,turn_end,agent_end,session_shutdown", "segment r2 holds only the resumed run", evs(r2));
check(r2.find(r => r.ev === "input")?.prelabel === "negation", "resumed correction pre-labelled", r2.find(r => r.ev === "input"));

// --- B2: the commands: `!` lines run the binary at expansion; only /rematch-mark is an input --
await run(["run", "--command", "rematch-status"]);
const s3 = sessionIds().at(-1) as string;
const expanded = store().query("SELECT p.data FROM part p JOIN message m ON m.id = p.message_id WHERE m.session_id = ? AND json_extract(m.data, '$.role') = 'user'").all(s3) as { data: string }[];
const expandedText = expanded.map(r => JSON.parse(r.data).text ?? "").join("\n");
check(expandedText.startsWith('<rematch-command name="status"/>') && expandedText.includes("entries"), "/rematch-status ran the binary before the model", expandedText.slice(0, 300));
check(!evs(trace(`oc:${s3}`)).includes("input"), "a command expansion is not a typed prompt", evs(trace(`oc:${s3}`)));
await run(["run", "--session", s3, "--command", "rematch-mark", "correction"]);
const marked = trace(`oc:${s3}:r2`);
check(marked.find(r => r.ev === "input")?.prelabel === "mark:correction" && marked.at(-1)?.flagged === true, "/rematch-mark recorded the mark and flagged the segment", marked);

// --- C: `opencode serve`: idle → new prompt in one process, then killed; reconciled next start
const port = 40000 + Math.floor(Math.random() * 20000);
const serve: ChildProcess = spawn(opencodeBin, ["serve", "--port", String(port), "--hostname", "127.0.0.1"], { cwd: proj, env, stdio: "ignore" });
const base = `http://127.0.0.1:${port}`;
for (let i = 0; i < 100; i++) {
	if (await fetch(`${base}/session`).then(r => r.ok).catch(() => false)) break;
	await Bun.sleep(200);
}
const created = (await fetch(`${base}/session?directory=${encodeURIComponent(proj)}`, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" }).then(r => r.json())) as { id: string };
const say = (text: string) =>
	fetch(`${base}/session/${created.id}/message?directory=${encodeURIComponent(proj)}`, {
		method: "POST",
		headers: { "content-type": "application/json" },
		body: JSON.stringify({ parts: [{ type: "text", text }] }),
	}).then(r => r.json());
await say("first served prompt");
await Bun.sleep(1500);
await say("second served prompt");
await Bun.sleep(1500);
const live = trace(`oc:${created.id}`);
check(evs(live).filter(e => e === "session_start").length === 1 && evs(live).filter(e => e === "input").length === 2, "idle → new prompt continues the same segment", evs(live));
check(evs(live).filter(e => e === "agent_end").length === 2 && !evs(live).includes("session_shutdown"), "two runs, segment still open while serve lives", evs(live));
check(fs.existsSync(path.join(root, "opencode/open", `oc:${created.id}.json`)), "serve process registered as owner");
serve.kill("SIGKILL");
await Bun.sleep(500);
await run(["run", "unrelated later prompt"]);
const settled = trace(`oc:${created.id}`);
check(settled.at(-1)?.ev === "session_shutdown" && evs(settled).filter(e => e === "session_shutdown").length === 1, "next start finalized the killed process's segment once", evs(settled));
check(!fs.existsSync(path.join(root, "traces", `oc:${created.id}:r2.jsonl`)), "no extra segment for the reconciled session");

// --- doctor sees the plugin declared in OpenCode's config ---------------------------------
const oc = rematch(["doctor"]).data.harnesses.opencode;
check(oc.status === "ok" && oc.plugin === fs.realpathSync(adapter), "doctor: opencode ok, plugin resolves to this adapter", oc);

// --- D: an approved personal fragment rides each main request's system prompt, traced once as sent
const frag = rematch(["create", "crate-tests-first", "--from", "description", "--description", "run crate tests before committing", "--editor", editorFragment]);
rematch(["activate", "crate-tests-first", "--unverified"]);
const fragApproved = approveAction(`instructions:${frag.data.id}`);
check(fragApproved.ok, "personal instructions approved by the user (reviewed snapshot)", fragApproved);
const fragFile = path.join(root, "staging/payments/crate-tests-first/AGENTS.fragment.md");
const fragBlock = `## rematch: crate-tests-first\n\n${fs.readFileSync(fragFile, "utf8").trim()}`;
await run(["run", "fragment carrying prompt"]);
const s5 = sessionIds().at(-1) as string;
check(systems.some(s => s.includes(fragBlock)), "the approved fragment reached the system prompt", systems.at(-1)?.slice(-600));
const sentRecs = trace(`oc:${s5}`).filter(r => r.ev === "fragments_sent");
const fragSha = createHash("sha256").update(fs.readFileSync(fragFile)).digest("hex");
check(
	sentRecs.length === 1 && sentRecs[0].host === "opencode" && sentRecs[0].fragments?.[0]?.id === frag.data.id && sentRecs[0].fragments?.[0]?.sha === fragSha,
	"fragments_sent: once, bound to the approved hash",
	sentRecs,
);

// --- count: each finished segment folded once ---------------------------------------------
const c1 = rematch(["count"]);
const folded: string[] = c1.data.folded ?? [];
check(folded.includes(`oc:${s1}`) && folded.includes(`oc:${s1}:r2`) && folded.includes(`oc:${created.id}`), "count folded every segment", folded);
const c2 = rematch(["count"]);
check((c2.data.folded ?? []).length === 0, "a second count folds nothing", c2.data.folded);
const sessionsRow = rematch(["import", "--history", "--harness", "opencode"]);
check(sessionsRow.ok, "import --history --harness opencode reads the isolated store", sessionsRow);

server.stop(true);
console.log(`\nOpenCode ${spawnSync(opencodeBin, ["--version"], { env, encoding: "utf8" }).stdout.trim()} · temp ${T}`);
console.log(failures === 0 ? "smoke: all checks passed" : `smoke: ${failures} check(s) failed`);
if (failures === 0 && !process.env.KEEP) fs.rmSync(T, { recursive: true, force: true });
process.exit(failures === 0 ? 0 : 1);
