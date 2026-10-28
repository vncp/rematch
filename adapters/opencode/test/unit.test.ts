import { describe, expect, test } from "bun:test";
import { userOnlyShellReason } from "../../../extension/index.ts";
import { commands, retrievalCacheValue, truncateToolOutputs } from "../index.ts";

type Msg = { info: Record<string, unknown>; parts: Record<string, unknown>[] };

const user = (text: string, synthetic = false): Msg => ({ info: { role: "user" }, parts: [{ type: "text", text, synthetic }] });
const result = (tool: string, output: string): Msg => ({
	info: { role: "assistant" },
	parts: [{ type: "tool", tool, callID: "c", state: { status: "completed", input: {}, output, time: { start: 1, end: 2 } } }],
});
const outputOf = (m: Msg | undefined): string => {
	const state = m?.parts[0]?.state;
	return state && typeof state === "object" && "output" in state && typeof state.output === "string" ? state.output : "";
};

describe("truncateToolOutputs", () => {
	test("shortens results older than max_age_turns typed user turns, newest untouched", () => {
		const long = "x".repeat(50);
		const msgs = [user("one"), result("bash", long), user("two"), user("injected", true), user("three"), result("bash", long)];
		const original = msgs[1];
		const n = truncateToolOutputs(msgs, [{ tool: "*", max_age_turns: 1, keep_chars: 10 }]);
		expect(n).toBe(1);
		expect(outputOf(msgs[1])).toBe(`${"x".repeat(10)}\n…[rematch: truncated 40 chars, age 2 turns]`);
		expect(outputOf(original)).toBe(long);
		expect(outputOf(msgs[5])).toBe(long);
	});

	test("the strictest matching rule wins; other tools and short outputs pass", () => {
		const msgs = [result("read", "y".repeat(30)), result("bash", "z".repeat(30)), result("bash", "ok"), user("a"), user("b"), user("c")];
		truncateToolOutputs(msgs, [
			{ tool: "bash", max_age_turns: 2, keep_chars: 20 },
			{ tool: "bash", max_age_turns: 1, keep_chars: 5 },
		]);
		expect(outputOf(msgs[0])).toBe("y".repeat(30));
		expect(outputOf(msgs[1]).startsWith("zzzzz\n…[rematch: truncated 25 chars")).toBe(true);
		expect(outputOf(msgs[2])).toBe("ok");
	});

	test("never splits a surrogate pair", () => {
		const msgs = [result("bash", `abcd😀${"e".repeat(20)}`), user("a"), user("b")];
		truncateToolOutputs(msgs, [{ tool: "*", max_age_turns: 1, keep_chars: 5 }]);
		expect(outputOf(msgs[0]).startsWith("abcd\n")).toBe(true);
	});
});

describe("approval guard (the shared parser OpenCode's tool.execute.before runs on bash)", () => {
	test("refuses shell approvals, withdrawals, marks and the switch", () => {
		for (const c of [
			"rematch library approve 01a",
			"/repo/target/release/rematch library decline 01a",
			"rematch --text library approve 01a",
			"cd x && rematch off",
			"rematch on",
			"sh adapters/claude-code/scripts/rematch.sh library approve 01a",
			"rematch library export --entry 01a --approve",
			"rematch library --text approve 01a",
			"rematch library approve --cwd=/tmp 01a",
			"rematch --cwd /tmp/x library decline 01a",
			"'/opt/my dir/rematch' library pin 01a",
			'"rematch" library disable 01a',
			"rematch library rollback 01a",
			"rematch library unpin 01a",
			"rematch uninstall --yes",
			"echo ok; rematch --text off",
			"x=$(rematch library approve 01a)",
			'sh -c "rematch --text off"',
			"REMATCH_ROOT=/x sudo rematch on",
			"rematch --text --cwd /w mark --class correction",
			"rematch --cwd=/w library export --approve-all",
			"rematch policy scope set bellwether-dev ~/repos/bellwether",
			"rematch --text policy scope unset bellwether-dev",
			"rematch policy auto add instructions **",
			"rematch --cwd /w policy auto remove scripts **",
			"rematch --text policy auto list",
			"rematch policy probes --daily 0",
			"rematch --cwd /w policy probes --max-parallel=4",
		]) {
			expect([c, userOnlyShellReason(c)]).toEqual([c, expect.stringContaining("reserved to the user")]);
		}
	});
	test("lets reads through", () => {
		for (const c of [
			"rematch status", "rematch library ls", "rematch library why oc:ses_1", "echo approve on", "rematch digest", "git log --oneline",
			"rematch --cwd library status", "rematch library export --entry 01a", 'git commit -m "let rematch off the hook"', "echo rematch off",
			"rematch policy show", "rematch policy probes", "rematch policy domain set github.com/acme/web web",
		]) {
			expect([c, userOnlyShellReason(c)]).toEqual([c, null]);
		}
		expect(userOnlyShellReason(undefined)).toBeNull();
	});
});

describe("commands", () => {
	test("each template is tagged and runs the binary before the model", () => {
		const c = commands("/opt/it's/rematch");
		expect(Object.keys(c).sort()).toEqual([
			"rematch-approve", "rematch-decline", "rematch-digest", "rematch-inspect", "rematch-library", "rematch-mark", "rematch-off", "rematch-on", "rematch-status", "rematch-why",
		]);
		expect(c["rematch-approve"]?.template).toStartWith('<rematch-command name="approve"/>\n!`\'/opt/it\'\\\'\'s/rematch\' --text library approve $ARGUMENTS`');
		expect(c["rematch-mark"]?.template).toStartWith('<rematch-command name="mark" args="$ARGUMENTS"/>');
		expect(c["rematch-mark"]?.template).not.toContain("!`");
	});
});

describe("retrieval cache", () => {
	test("caches only a successful answer for a recorded prompt", () => {
		expect(retrievalCacheValue({ kind: "retrieve", has_input: true, context: "units" })).toBe("units");
		expect(retrievalCacheValue({ kind: "retrieve", has_input: true })).toBeNull();
		// Asked before the prompt reached the trace, failed, timed out (no reply), or off: ask again.
		expect(retrievalCacheValue({ kind: "retrieve", has_input: false })).toBeUndefined();
		expect(retrievalCacheValue({ kind: "retrieve", error: "lock timeout" })).toBeUndefined();
		expect(retrievalCacheValue(undefined)).toBeUndefined();
		expect(retrievalCacheValue({ kind: "retrieve", off: true })).toBeUndefined();
	});
});
