import { afterAll, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { idFileStem, resolveBinary, skillFromReadPath } from "./index";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rematch windows names "));
afterAll(() => fs.rmSync(tmp, { recursive: true, force: true }));

test("Windows trace filenames retain logical harness identities without reserved characters", () => {
	const cases = [
		["cc:abc", "cc%3Aabc"],
		["pi:abc:r2", "pi%3Aabc%3Ar2"],
		["oc:ses_1:r3", "oc%3Ases_1%3Ar3"],
		["codex:abc", "codex%3Aabc"],
		["cc%3Aabc", "cc%253Aabc"],
		["CON", "%43ON"],
		["nul.txt", "%6Eul.txt"],
		["COM1.log", "%43OM1.log"],
		["COM10", "COM10"],
		["session. ", "session.%20"],
		[".hidden", "%2Ehidden"],
		["..", "%2E%2E"],
		["com0", "%63om0"],
		["NUL .txt", "%4EUL .txt"],
		["ordinary-uuid", "ordinary-uuid"],
	] as const;
	for (const [logical, expected] of cases) {
		expect(idFileStem(logical, "win32")).toBe(expected);
		expect(idFileStem(logical, "darwin")).toBe(logical);
		const file = path.join(tmp, `${expected}.jsonl`);
		fs.writeFileSync(file, JSON.stringify({ session_id: logical }) + "\n");
		expect(JSON.parse(fs.readFileSync(file, "utf8")).session_id).toBe(logical);
	}
	expect(idFileStem("a<b>c:d/e\\f|g?h*i%j\u0001", "win32")).not.toMatch(/[<>:"/\\|?*\u0000-\u001f]/);
});

test("binary discovery and skill paths work in space-containing native paths", () => {
	const bin = path.join(tmp, process.platform === "win32" ? "rematch.exe" : "rematch");
	fs.writeFileSync(bin, "test executable lookup");
	fs.chmodSync(bin, 0o755);
	expect(resolveBinary({ PATH: tmp }, path.join(tmp, "absent extension"))).toBe(bin);
	expect(skillFromReadPath("C:\\Users\\Test User\\skills\\quant-review\\SKILL.md")).toBe("quant-review");
	expect(skillFromReadPath("C:\\Users\\Test User\\skills\\quant-review\\SKILL.md:1-30")).toBe("quant-review");
});
