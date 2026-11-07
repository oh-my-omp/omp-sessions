// session-urls filtered reads: loadFiltered builds in full only the entries a
// filtered timeline or events.jsonl prints, and both render exactly as the fully
// built session filtered afterwards, including the clock-only window's date and
// its ambiguity error.
import { afterAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { loadSession } from "../src/events.ts";
import { type EventFilter, loadFiltered, parseTypeFilter } from "../src/filter.ts";
import { eventRecords, renderTimeline } from "../src/render.ts";
import { parseWindow, resolveWindow } from "../src/window.ts";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-filtered-"));
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }));

const NOW = Date.UTC(2026, 10, 3, 12, 0, 0);

/** Local wall time on 2026-11-0<day> as an ISO stamp, so dates hold in any zone. */
function local(day: number, hour: number, minute: number): string {
	return new Date(2026, 10, day, hour, minute).toISOString();
}

function session(days: readonly number[]): string {
	const records: unknown[] = [{ type: "session", version: 3, id: "sid", timestamp: local(days[0] ?? 1, 9, 0), cwd: "/w" }];
	let parent: string | null = null;
	let n = 0;
	const add = (day: number, hour: number, minute: number, rest: Record<string, unknown>): void => {
		const id = `e${n++}`;
		records.push({ id, parentId: parent, timestamp: local(day, hour, minute), ...rest });
		parent = id;
	};
	for (const day of days)
		for (let hour = 9; hour < 13; hour++) {
			add(day, hour, 0, { type: "message", message: { role: "user", content: `prompt ${day}/${hour}` } });
			add(day, hour, 1, {
				type: "message",
				message: {
					role: "assistant",
					content: [
						{ type: "thinking", thinking: `thinking ${hour}` },
						{ type: "text", text: `answer ${hour}` },
						{ type: "toolCall", id: `c${n}`, name: hour % 2 === 0 ? "bash" : "read", arguments: { command: "ls", path: "/p" } },
					],
				},
			});
			add(day, hour, 2, { type: "message", message: { role: "toolResult", toolCallId: `c${n - 1}`, toolName: "bash", content: "out" } });
			add(day, hour, 3, {
				type: "message",
				message: {
					role: "toolResult",
					toolName: "task",
					content: `<task-result id="sub-${day}-${hour}" agent="Alpha" status="completed" duration="1s"><output>sub ${hour}</output></task-result>`,
				},
			});
			add(day, hour, 20, { type: "compaction", summary: `summary ${hour}` });
		}
	const file = path.join(dir, `${days.join("-")}.jsonl`);
	fs.writeFileSync(file, `${records.map((record) => JSON.stringify(record)).join("\n")}\n`);
	return file;
}

const ONE_DAY = session([1]);
const TWO_DAYS = session([1, 2]);

const CASES: readonly (readonly [string | null, string | null, string | null])[] = [
	["thinking", null, null],
	["call:bash,say", null, null],
	["-call", null, null],
	[null, "1000", "1130"],
	[null, "2026-11-01T1000", null],
	["marker:compaction", "1000", null],
	["user", null, "2026-11-01T1100"],
	["subresult", null, null],
];

/** The fully built session with the filter applied afterwards, as before pushdown. */
async function expected(file: string, types: string | null, since: string | null, until: string | null) {
	const loaded = await loadSession(file, "sid", "");
	const spec = parseWindow(since, until);
	const filter: EventFilter = {
		types: types === null ? null : parseTypeFilter([types]),
		window: spec === null ? null : resolveWindow(spec, loaded.events, NOW),
	};
	return {
		timeline: renderTimeline(loaded, "session://sid", "session://sid", null, filter),
		records: [...eventRecords(loaded, "session://sid", filter)],
	};
}

describe("session-urls filtered reads", () => {
	for (const [types, since, until] of CASES)
		test(`type=${types} since=${since} until=${until} renders as the fully built session`, async () => {
			const want = await expected(ONE_DAY, types, since, until);
			const typeFilter = types === null ? null : parseTypeFilter([types]);
			const spec = parseWindow(since, until);
			const timeline = await loadFiltered(ONE_DAY, "sid", "", typeFilter, spec, NOW, "timeline");
			expect(renderTimeline(timeline.loaded, "session://sid", "session://sid", null, timeline.filter)).toBe(want.timeline);
			const records = await loadFiltered(ONE_DAY, "sid", "", typeFilter, spec, NOW, "records");
			expect([...eventRecords(records.loaded, "session://sid", records.filter)]).toEqual(want.records);
		});

	test("records with only a dated window build no entry outside it", async () => {
		const spec = parseWindow("2026-11-01T1000", "2026-11-01T1030");
		const { loaded } = await loadFiltered(ONE_DAY, "sid", "", null, spec, NOW, "records");
		expect(new Set(loaded.events.map((event) => new Date(event.t).getHours()))).toEqual(new Set([10]));
	});

	test("type=subresult keeps the session's subagent results", async () => {
		const want = await expected(ONE_DAY, "subresult", null, null);
		expect(want.records.length).toBeGreaterThan(0);
		expect(want.records.every((record) => record.kind === "subresult")).toBe(true);
		expect(want.timeline).toContain("sub-1-9");
		const got = await loadFiltered(ONE_DAY, "sid", "", parseTypeFilter(["subresult"]), null, NOW, "records");
		expect([...eventRecords(got.loaded, "session://sid", got.filter)]).toEqual(want.records);
	});

	test("a bare time on a two-day session fails as before", async () => {
		const spec = parseWindow("1000", null);
		await expect(loadFiltered(TWO_DAYS, "sid", "", null, spec, NOW, "timeline")).rejects.toThrow("A bare time is ambiguous");
		await expect(loadFiltered(TWO_DAYS, "sid", "", null, spec, NOW, "records")).rejects.toThrow("A bare time is ambiguous");
	});
});
