// Pure helpers of the Pi adapter (bun test). The real-Pi acceptance runs in pi-process.test.ts.
import { afterEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { hookFromModule, userOnlyShellReason } from "../../extension/index.ts";
import {
	binaryEnv,
	finished,
	latestSegment,
	parseHookModule,
	piLoadedEntries,
	planTrace,
	refusedForModel,
	restoreState,
	unrecordedLoad,
	retrievalMessage,
	segmentId,
	traceId,
} from "./index.ts";

const tmps: string[] = [];
const tmp = () => {
	const d = fs.mkdtempSync(path.join(os.tmpdir(), "rematch-pi-unit-"));
	tmps.push(d);
	return d;
};
afterEach(() => {
	for (const d of tmps.splice(0)) fs.rmSync(d, { recursive: true, force: true });
});

const write = (dir: string, id: string, recs: object[]) => fs.writeFileSync(path.join(dir, `${id}.jsonl`), recs.map(r => `${JSON.stringify(r)}\n`).join(""));

describe("identity and segments", () => {
	test("ids are sanitized like rematch-core import_pi::session_id", () => {
		expect(traceId("019a-b")).toBe("pi:019a-b");
		expect(traceId("a/b c")).toBe("pi:a_b_c");
		expect(segmentId("pi:x", 1)).toBe("pi:x");
		expect(segmentId("pi:x", 3)).toBe("pi:x:r3");
	});

	test("no trace → new; unfinished → continue (reload writes no start); finished → next segment", () => {
		const dir = tmp();
		expect(planTrace(dir, "pi:s", "startup")).toEqual({ id: "pi:s", continues: false, writeStart: true, previous: null });
		write(dir, "pi:s", [{ seq: 0, ev: "session_start" }, { seq: 1, ev: "input", text: "a" }]);
		expect(planTrace(dir, "pi:s", "reload")).toMatchObject({ id: "pi:s", continues: true, writeStart: false });
		expect(planTrace(dir, "pi:s", "resume")).toMatchObject({ id: "pi:s", continues: true, writeStart: true });
		write(dir, "pi:s", [{ seq: 0, ev: "session_start" }, { seq: 1, ev: "session_shutdown" }]);
		expect(planTrace(dir, "pi:s", "resume")).toMatchObject({ id: "pi:s:r2", continues: false, writeStart: true });
		write(dir, "pi:s:r2", [{ seq: 0, ev: "session_start" }, { seq: 1, ev: "session_shutdown" }, { seq: 2, ev: "session_start" }]);
		expect(latestSegment(dir, "pi:s")).toEqual({ n: 2, id: "pi:s:r2" });
		expect(finished(fs.readFileSync(path.join(dir, "pi:s:r2.jsonl"), "utf8").trim().split("\n").map(l => JSON.parse(l)))).toBe(false);
		expect(planTrace(dir, "pi:s", "startup")).toMatchObject({ id: "pi:s:r2", continues: true, writeStart: true });
	});

	test("restoreState replays seq, context, flag reasons and the stuck window", () => {
		const st = restoreState(
			[
				{ seq: 0, ev: "session_start", model: { provider: "p", id: "m" } },
				{ seq: 1, ev: "input", text: "do it", prelabel: "none" },
				{ seq: 2, ev: "tool_result", tool: "bash", args_hash: "h", is_error: true, input: { command: "x" } },
				{ seq: 3, ev: "tool_result", tool: "bash", args_hash: "h", is_error: true, input: { command: "x" } },
				{ seq: 4, ev: "tool_blocked", tool: "bash", args_hash: "h", hook_id: "stk" },
				{ seq: 5, ev: "tool_blocked", tool: "bash", args_hash: "h", hook_id: "blk" },
				{ seq: 6, ev: "tool_result", tool: "edit", args_hash: "e", is_error: true, input: { path: "a.ts" } },
				{ seq: 7, ev: "tool_result", tool: "write", args_hash: "w", is_error: false, input: { path: "b.ts" } },
				{ seq: 8, ev: "turn_end", model: { provider: "p", id: "m2" }, text_head: "done", stop_reason: "stop" },
				{ seq: 9, ev: "input", text: "no, wrong", prelabel: "negation" },
				{ seq: 10, ev: "mark", class: "scope" },
			],
			new Set(["stk"]),
		);
		expect(st.seq).toBe(11);
		expect([st.lastInput, st.lastDeliverable, st.lastStop, st.turnsSinceAgentEnd]).toEqual(["no, wrong", "done", "stop", 1]);
		expect(st.lastModel).toEqual({ provider: "p", id: "m2" });
		expect([...st.reasons]).toEqual(["tool_error_cluster", "redirect:negation", "mark:scope"]);
		expect([...st.changed]).toEqual(["b.ts"]);
		expect(st.callKeys).toEqual(["bash\0h", "bash\0h", "bash\0h", "edit\0e", "write\0w"]);
	});
});

describe("loaded-set credit", () => {
	test("a reload or in-place re-match records a session_start only for a new entry, version or hook", () => {
		const recs = [
			{ seq: 0, ev: "session_start", loaded_entries: [{ id: "s", version: 1 }], hooks_loaded: ["r"] },
			{ seq: 1, ev: "input", text: "x" },
		];
		expect(unrecordedLoad(recs, [{ id: "s", version: 1 }], ["r"])).toBe(false);
		expect(unrecordedLoad(recs, [], [])).toBe(false); // a withdrawal adds nothing to credit
		expect(unrecordedLoad(recs, [{ id: "s", version: 1 }, { id: "h", version: 1 }], ["r", "h"])).toBe(true);
		expect(unrecordedLoad(recs, [{ id: "s", version: 2 }], ["r"])).toBe(true);
		const after = [...recs, { seq: 2, ev: "session_start", loaded_entries: [{ id: "h", version: 1 }], hooks_loaded: ["h"] }];
		expect(unrecordedLoad(after, [{ id: "s", version: 1 }, { id: "h", version: 1 }], ["r", "h"])).toBe(false);
	});
});

describe("hooks and authority", () => {
	test("generated modules parse as data (library::hook_module shape)", () => {
		const text = `// Generated by rematch for entry no-force-push (e1). Approved block hook: data only.\nexport default {\n  id: "e1",\n  mode: "block",\n  tool: "bash",\n  field: "command",\n  regex: "(?i)git\\\\s+push\\\\s+--force",\n  message: "Use --force-with-lease.",\n  pass_condition: "p",\n  params: {"flags":"i","source":"git\\\\s+push\\\\s+--force"},\n};\n`;
		const hook = hookFromModule(parseHookModule(text), "x");
		expect(hook).toMatchObject({ id: "e1", mode: "block", tool: "bash", field: "command", message: "Use --force-with-lease." });
		expect(hook.pattern?.test("GIT push --force origin")).toBe(true);
		expect(() => parseHookModule("module.exports = {}")).toThrow("no default export");
	});

	test("the model cannot approve, decline, withdraw, mark or switch rematch off", () => {
		for (const [op, args] of [["library", ["approve", "e1"]], ["library", ["decline", "e1"]], ["library", ["disable", "e1"]], ["library", ["rollback", "e1"]], ["library", ["export", "--entry", "e1", "--approve"]], ["library", ["export", "--approve-all"]], ["off", []], ["on", []], ["mark", ["correction"]], ["uninstall", []], ["library", ["--text", "approve", "e1"]], ["library", ["--cwd", "/x", "disable", "e1"]], ["library", ["--cwd=/x", "--text", "rollback", "e1"]], ["library", ["--text", "export", "--approve-all"]], ["off", ["--text"]], ["policy", ["auto", "add", "instructions", "**"]], ["policy", ["auto", "remove", "scripts", "**"]], ["policy", ["--text", "auto", "list"]], ["policy", ["probes", "--daily", "0"]], ["policy", ["--text", "probes", "--max-parallel=4"]]] as const) {
			expect(refusedForModel(op, args)).toContain("reserved to the user");
		}
		expect(refusedForModel("library", ["ls"])).toBeNull();
		expect(refusedForModel("status", [])).toBeNull();
		expect(refusedForModel("policy", ["probes"])).toBeNull();
		// The forms clap accepts: global flags before and after `library`, `--x=y`, paths, quotes, env, sh -c.
		for (const cmd of [
			"rematch library approve e1",
			"cd x && /opt/bin/rematch --text library decline e1",
			"REMATCH_ROOT=/r rematch off",
			"rematch library --text approve e1",
			"rematch library approve --text e1",
			"rematch --cwd /tmp/x library pin e1",
			"rematch --cwd=/tmp/x library unpin e1",
			"rematch --text --cwd /w library --cwd /w disable e1",
			'"rematch" library rollback e1',
			"'/usr/local/bin/rematch' uninstall --yes",
			"env REMATCH_ROOT=/r rematch on",
			"sh -c 'rematch library approve e1'",
			"echo ok; $(rematch library approve e1)",
			"true\nrematch off",
			"./target/debug/rematch.sh library decline e1",
			"rematch library export --entry e1 --approve",
			"rematch --text library export --approve-all",
			"rematch --cwd /w mark --class correction",
			"rematch --text library -- rollback e1",
			"C:\\tools\\rematch.exe --cwd C:\\w library disable e1",
			"{ rematch on; }",
			"rematch --cwd /w policy scope set bellwether-dev github.com/vncp/bellwether --subpath crates",
			"rematch policy --text scope unset bellwether-dev",
			"rematch policy auto add instructions **",
			"rematch --cwd /w policy --text auto remove scripts **",
			"rematch policy auto list",
			"rematch policy probes --daily 0",
			"rematch --cwd /w policy --text probes --max-parallel=4",
		]) {
			expect([cmd, userOnlyShellReason(cmd)]).toEqual([cmd, expect.stringContaining("reserved to the user")]);
		}
		for (const cmd of [
			"rematch library ls",
			"rematch library why e1",
			"rematch --text status",
			"grep -n 'library approve' docs/DESIGN.md",
			'git commit -m "let rematch off the hook"',
			"cat rematch.log",
			"rematch-lint library approve e1",
			"rematch library export --entry e1",
			"rematch policy show",
			"rematch policy probes",
		]) {
			expect([cmd, userOnlyShellReason(cmd)]).toEqual([cmd, null]);
		}
	});

	test("loaded entries: predicate hits minus retrieve hooks, agents, lessons and unloaded hooks", () => {
		const hooks = [hookFromModule({ id: "r", mode: "retrieve", tool: "*", params: { budget_bytes: 10, min_overlap: 2 } }, "r")];
		const data = {
			entries: [
				{ id: "s", kind: "skill", predicate_hit: true, version: 2, sha: "ab" },
				{ id: "miss", kind: "skill", predicate_hit: false },
				{ id: "r", kind: "hook", predicate_hit: true },
				{ id: "h", kind: "hook", predicate_hit: true },
				{ id: "a", kind: "agent", predicate_hit: true },
			],
		};
		expect(piLoadedEntries(data, hooks)).toEqual([{ id: "s", version: 2, sha: "ab" }]);
	});
});

describe("process boundary and messages", () => {
	test("Pi's agent dir never reaches the binary as omp's PI_CODING_AGENT_DIR", () => {
		const env = binaryEnv({ PATH: "/bin", PI_CODING_AGENT_DIR: "/p/agent", PI_CODING_AGENT_SESSION_DIR: "/p/s" }, "/p/agent");
		expect(env).toEqual({ PATH: "/bin", REMATCH_PI_AGENT_DIR: "/p/agent", REMATCH_PI_SESSION_DIR: "/p/s" });
	});

	test("the retrieval message is a hidden Pi custom message", () => {
		const m = retrievalMessage([{ entry_id: "e", name: "n", path: "units/a.md", title: "t", text: "body\n" }], 5);
		expect(m).toMatchObject({ role: "custom", customType: "com.vncp.rematch.retrieval", display: false, timestamp: 5, details: { units: [{ entry_id: "e", path: "units/a.md" }] } });
		expect(String(m.content)).toContain('<unit skill="n" path="units/a.md">\nbody\n</unit>');
		expect(String(m.content)).not.toContain("skill://");
	});
});
