// session-urls session listing: since/until filter before `limit` cuts the
// result, an unfiltered listing stops at `limit`, and text matches the title or
// prompt words before the limit applies.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { __resetDirsFromEnvForTests, setAgentDir } from "@oh-my-pi/pi-utils";
import { type SessionQuery, sessionRows } from "../src/relations.ts";

const agentDirBefore = process.env.PI_CODING_AGENT_DIR;
const agent = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-rows-"));
const bucket = path.join(agent, "sessions", "--proj--");
fs.mkdirSync(bucket, { recursive: true });

const BASE = Date.UTC(2026, 10, 1, 10, 0);

/** A session file listed as modified at `BASE + minute` minutes, titled `title`. */
function session(id: string, minute: number, title: string): void {
	const at = new Date(BASE + minute * 60_000);
	const file = path.join(bucket, `${id}.jsonl`);
	fs.writeFileSync(
		file,
		`${JSON.stringify({ type: "session", version: 3, id, timestamp: at.toISOString(), cwd: "/w", title })}\n${JSON.stringify({
			type: "message",
			message: { role: "user", content: `prompt ${title}` },
		})}\n`,
	);
	fs.utimesSync(file, at, at);
}

session("s1", 4, "alpha report");
session("s2", 3, "beta report");
session("s3", 2, "gamma notes");
session("s4", 1, "delta notes");

beforeAll(() => setAgentDir(agent));
afterAll(() => {
	if (agentDirBefore === undefined) delete process.env.PI_CODING_AGENT_DIR;
	else process.env.PI_CODING_AGENT_DIR = agentDirBefore;
	__resetDirsFromEnvForTests();
	fs.rmSync(agent, { recursive: true, force: true });
});

async function rows(rest: Partial<SessionQuery>): Promise<string[]> {
	const query: SessionQuery = { project: null, text: null, since: null, until: null, limit: 200, ...rest };
	return (await sessionRows(query, undefined)).map((row) => row.sid);
}

describe("session rows", () => {
	test("newest first, the limit cuts after ordering", async () => {
		expect(await rows({})).toEqual(["s1", "s2", "s3", "s4"]);
		expect(await rows({ limit: 2 })).toEqual(["s1", "s2"]);
	});

	test("since and until filter before the limit cuts", async () => {
		expect(await rows({ since: BASE + 3 * 60_000 })).toEqual(["s1", "s2"]);
		expect(await rows({ since: BASE + 3 * 60_000, limit: 1 })).toEqual(["s1"]);
		expect(await rows({ until: BASE + 3 * 60_000 })).toEqual(["s3", "s4"]);
		expect(await rows({ until: BASE + 3 * 60_000, limit: 1 })).toEqual(["s3"]);
		expect(await rows({ since: BASE + 2 * 60_000, until: BASE + 3 * 60_000 })).toEqual(["s3"]);
	});

	test("text matches title and prompt, then the limit cuts", async () => {
		expect(await rows({ text: "gamma notes" })).toEqual(["s3"]);
		expect(await rows({ text: "report" })).toEqual(["s1", "s2"]);
		expect(await rows({ text: "report", limit: 1 })).toEqual(["s1"]);
	});
});
