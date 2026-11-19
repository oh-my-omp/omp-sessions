// session:// resolution of session files whose names carry no timestamp. Such a
// file is a session when its header reads with an id and a valid creation time,
// and its id and date come from that header. A stamped name keeps its own id and
// date, and wins over a header-named copy of the same id.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { __resetDirsFromEnvForTests, setAgentDir } from "@oh-my-pi/pi-utils";
import { bucketSessions, locateSession } from "../src/routes.ts";

const agentDirBefore = process.env.PI_CODING_AGENT_DIR;
const agent = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-names-"));
const bucket = path.join(agent, "sessions", "--proj--");
fs.mkdirSync(bucket, { recursive: true });

/** A session file whose header says `id` and was created at `created`. */
function session(id: string | undefined, created: string | undefined): string {
	const record = { type: "session", version: 3, id, timestamp: created, cwd: "/w" };
	return `${JSON.stringify(record)}\n${JSON.stringify({ type: "message", message: { role: "user", content: "hi" } })}\n`;
}

const STAMPED = "2026-11-14T10-00-00-000Z_aaa111.jsonl";
fs.writeFileSync(path.join(bucket, STAMPED), session("zzz999", "2026-11-14T10:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "current.jsonl"), session("bbb222", "2026-11-13T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "copy.jsonl"), session("aaa111", "2026-11-10T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "near.jsonl"), session("aaa999", "2026-11-11T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "left.jsonl"), session("ccc100", "2026-11-12T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "right.jsonl"), session("ccc200", "2026-11-12T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "twin-b.jsonl"), session("fff500", "2026-11-16T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "twin-a.jsonl"), session("fff500", "2026-11-16T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "broken.jsonl"), "not json\n{}\n");
fs.writeFileSync(path.join(bucket, "no-created.jsonl"), session("ddd300", undefined));
fs.writeFileSync(path.join(bucket, "no-id.jsonl"), session(undefined, "2026-11-12T12:00:00.000Z"));
fs.writeFileSync(path.join(bucket, "notes.txt"), session("eee400", "2026-11-12T12:00:00.000Z"));
fs.mkdirSync(path.join(bucket, "folder.jsonl"));
// A bucket the user cannot list; a lookup across buckets skips it.
const locked = path.join(agent, "sessions", "--locked--");
fs.mkdirSync(locked);
fs.writeFileSync(path.join(locked, "hidden.jsonl"), session("ggg700", "2026-11-12T12:00:00.000Z"));
fs.chmodSync(locked, 0o000);

beforeAll(() => setAgentDir(agent));
afterAll(() => {
	fs.chmodSync(locked, 0o755);
	if (agentDirBefore === undefined) delete process.env.PI_CODING_AGENT_DIR;
	else process.env.PI_CODING_AGENT_DIR = agentDirBefore;
	__resetDirsFromEnvForTests();
	fs.rmSync(agent, { recursive: true, force: true });
});

const named = { kind: "named", name: "--proj--" } as const;

describe("bucket session files", () => {
	test("a stamped name is a session by name; other .jsonl files are sessions by header", async () => {
		const sessions = await bucketSessions(bucket);
		expect(path.basename(sessions[0]?.file ?? "")).toBe(STAMPED);
		expect(sessions.map((session) => session.sid).sort()).toEqual([
			"aaa111",
			"aaa111",
			"aaa999",
			"bbb222",
			"ccc100",
			"ccc200",
			"fff500",
			"fff500",
		]);
		const current = sessions.find((session) => session.sid === "bbb222");
		expect(current?.created.toISOString()).toBe("2026-11-13T12:00:00.000Z");
	});

	test("a file whose header lacks an id or a valid creation time is not a session", async () => {
		const names = (await bucketSessions(bucket)).map((session) => path.basename(session.file));
		for (const excluded of ["broken.jsonl", "no-created.jsonl", "no-id.jsonl", "folder.jsonl", "notes.txt"]) {
			expect(names).not.toContain(excluded);
		}
	});

	test("a listed date URL reaches a header-named session by its header date", async () => {
		const ref = await locateSession({ kind: "listed", bucket: named, date: "2026-11-13", sid: "bbb222" }, undefined);
		expect(path.basename(ref.file)).toBe("current.jsonl");
		const copy = await locateSession({ kind: "listed", bucket: named, date: "2026-11-10", sid: "aaa111" }, undefined);
		expect(path.basename(copy.file)).toBe("copy.jsonl");
	});

	test("a date URL of an id held by two same-day header-named files opens the one whose name sorts first", async () => {
		const ref = await locateSession({ kind: "listed", bucket: named, date: "2026-11-16", sid: "fff500" }, undefined);
		expect(path.basename(ref.file)).toBe("twin-a.jsonl");
	});
});

describe("sid prefix resolution", () => {
	test("a header-named session resolves by its header id prefix", async () => {
		const ref = await locateSession({ kind: "prefix", prefix: "bbb" }, undefined);
		expect(ref.sid).toBe("bbb222");
		expect(path.basename(ref.file)).toBe("current.jsonl");
	});

	test("one header-named match resolves; several matches are ambiguous", async () => {
		expect(path.basename((await locateSession({ kind: "prefix", prefix: "ccc1" }, undefined)).file)).toBe("left.jsonl");
		await expect(locateSession({ kind: "prefix", prefix: "ccc" }, undefined)).rejects.toThrow('"ccc" matches 2 sessions');
	});

	test("a stamped name wins over a header-named copy of the same id", async () => {
		const ref = await locateSession({ kind: "prefix", prefix: "aaa1" }, undefined);
		expect(path.basename(ref.file)).toBe(STAMPED);
		expect(ref.sid).toBe("aaa111");
	});

	test("a stamped match and a header-named match of another id are ambiguous", async () => {
		await expect(locateSession({ kind: "prefix", prefix: "aaa" }, undefined)).rejects.toThrow('"aaa" matches 2 sessions');
	});

	test("a stamped name is matched by its name, not its header id", async () => {
		await expect(locateSession({ kind: "prefix", prefix: "zzz" }, undefined)).rejects.toThrow('No session id starts with "zzz"');
	});

	test("a bucket that cannot be listed is skipped by a lookup across buckets", async () => {
		const ref = await locateSession({ kind: "prefix", prefix: "bbb" }, undefined);
		expect(path.basename(ref.file)).toBe("current.jsonl");
	});

	test("files that are not sessions match no prefix", async () => {
		for (const prefix of ["broken", "ddd", "eee", "folder"]) {
			await expect(locateSession({ kind: "prefix", prefix }, undefined)).rejects.toThrow(`No session id starts with "${prefix}"`);
		}
	});
});
