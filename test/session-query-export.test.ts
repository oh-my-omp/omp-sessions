// session_query export guard: a file below the sessions directory is refused
// under every spelling that reaches it, and a file outside is written.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { DuckDBInstance } from "@duckdb/node-api";
import { __resetDirsFromEnvForTests, setAgentDir } from "@oh-my-pi/pi-utils";
import { exportResult } from "../src/query/run.ts";

const agentDirBefore = process.env.PI_CODING_AGENT_DIR;
const root = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-export-"));
const agent = path.join(root, "agent");
const sessions = path.join(agent, "sessions");
const outside = path.join(root, "out");
fs.mkdirSync(sessions, { recursive: true });
fs.mkdirSync(outside);
const instance = await DuckDBInstance.create(":memory:");
const connection = await instance.connect();

beforeAll(() => setAgentDir(agent));
afterAll(() => {
	connection.closeSync();
	instance.closeSync();
	if (agentDirBefore === undefined) delete process.env.PI_CODING_AGENT_DIR;
	else process.env.PI_CODING_AGENT_DIR = agentDirBefore;
	__resetDirsFromEnvForTests();
	fs.rmSync(root, { recursive: true, force: true });
});

const exportTo = (target: string) => exportResult(connection, "SELECT 1 AS n", null, target, "json");

describe("export guard", () => {
	test("refuses a file in the sessions directory", async () => {
		const target = path.join(sessions, "x.json");
		await expect(exportTo(target)).rejects.toThrow("inside the omp sessions directory");
		expect(fs.existsSync(target)).toBe(false);
	});

	test("refuses a file reached through a symbolic link to the sessions directory", async () => {
		const link = path.join(root, "link");
		fs.symlinkSync(sessions, link, "dir");
		await expect(exportTo(path.join(link, "x.json"))).rejects.toThrow("inside the omp sessions directory");
		expect(fs.existsSync(path.join(sessions, "x.json"))).toBe(false);
	});

	test("refuses an outside name that is a symbolic link to a session file", async () => {
		const file = path.join(sessions, "s.jsonl");
		fs.writeFileSync(file, "{}\n");
		const link = path.join(outside, "s.json");
		fs.symlinkSync(file, link);
		await expect(exportTo(link)).rejects.toThrow("inside the omp sessions directory");
		expect(fs.readFileSync(file, "utf8")).toBe("{}\n");
	});

	test("writes a file outside the sessions directory", async () => {
		const target = path.join(outside, "result.json");
		const result = await exportTo(target);
		expect(result.path).toBe(target);
		expect(fs.readFileSync(target, "utf8")).toContain('"n":1');
	});
});
