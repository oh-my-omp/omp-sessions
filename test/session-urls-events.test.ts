// session-urls session loading: the index pass picks the same branch, turns and
// call completions the whole-file projection did; tool output is measured to the
// same length contentText gave; the build pass reads only the branch entries a
// caller asks for, LIGHT keeps every structural field of FULL, and subagent
// results deduplicate over the whole branch whatever is skipped.
import { afterAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { type Build, buildEvents, indexSession, loadSession, type SessionEvent } from "../src/events.ts";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-events-"));
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }));

let files = 0;
function sessionFile(records: readonly unknown[]): string {
	const file = path.join(dir, `s${files++}.jsonl`);
	fs.writeFileSync(file, `${records.map((record) => JSON.stringify(record)).join("\n")}\n`);
	return file;
}

const SPEECH = 1 << 20;
const HEADER = { type: "session", version: 3, id: "sid", timestamp: "2026-11-01T10:00:00.000Z", cwd: "/w" };

/** textChars(contentText(content)) of the whole-file projection, from the parsed record. */
function expectedChars(content: unknown): number {
	if (typeof content === "string") return content.length;
	if (!Array.isArray(content)) return 0;
	const parts: string[] = [];
	let chars = 0;
	let clipped = false;
	for (const block of content.slice(0, 1000)) {
		if (block === null || typeof block !== "object" || Array.isArray(block)) continue;
		const fields = block as Record<string, unknown>;
		if (fields.type === "image") parts.push("[image]");
		if (typeof fields.text !== "string") continue;
		chars += fields.text.length;
		if (fields.text.length > SPEECH) clipped = true;
		parts.push(fields.text);
	}
	return clipped ? chars : parts.join("\n").length;
}

function entry(id: string, parentId: string | null, minute: number, rest: Record<string, unknown>): Record<string, unknown> {
	return { id, parentId, timestamp: `2026-11-01T10:${String(minute).padStart(2, "0")}:00.000Z`, ...rest };
}

function call(id: string, name: string, args: Record<string, unknown>): Record<string, unknown> {
	return { type: "toolCall", id, name, arguments: args };
}

function result(callId: string, toolName: string, content: unknown, isError = false): Record<string, unknown> {
	return { type: "message", message: { role: "toolResult", toolCallId: callId, toolName, isError, content } };
}

const TASK_RESULT = '<task-result id="Kid" agent="task" status="completed" duration="3s">\n<output>\nkid done\n</output>\n</task-result>';

const K1_OUTPUT = [{ type: "text", text: "line 1\nline ✓ 😀" }, { type: "image" }, { type: "text", text: "" }];

/** Every kind of entry, a duplicate subagent result, an abandoned branch with a prompt, and results off the branch. */
const MIXED = [
	{ type: "title", v: 1, title: "Mixed" },
	HEADER,
	entry("a", null, 0, { type: "model_change", model: "m/x", role: "default" }),
	entry("b", "a", 1, { type: "message", message: { role: "user", content: "first prompt with artifact://3" } }),
	entry("c", "b", 2, {
		type: "message",
		message: {
			role: "assistant",
			content: [
				{ type: "thinking", thinking: "think" },
				{ type: "thinking", thinking: "" },
				{ type: "redactedThinking" },
				{ type: "text", text: "say it" },
				call("k1", "bash", { command: "ls", i: "Listing" }),
				call("k2", "write", { path: "agent://Peer", content: "hello peer" }),
				call("k3", "task", { tasks: [{ name: "Kid", agent: "task", task: "do it" }], context: "ctx" }),
				call("k4", "read", { path: "/x" }),
			],
		},
	}),
	entry("s1", "c", 2, {
		type: "custom",
		customType: "tool_execution_start",
		data: { toolCallId: "k1", startedAt: "2026-11-01T10:02:01.000Z", intent: "Started" },
	}),
	entry("d", "s1", 3, result("k1", "bash", K1_OUTPUT)),
	entry("e", "d", 3, result("k2", "write", "ok")),
	entry("f", "e", 4, result("k3", "task", [{ type: "text", text: TASK_RESULT }])),
	entry("g", "f", 5, { type: "custom_message", customType: "async-result", content: TASK_RESULT }),
	entry("h", "g", 6, { type: "message", message: { role: "user", content: [{ type: "text", text: "injected" }], attribution: "agent" } }),
	entry("i", "h", 7, { type: "message", message: { role: "developer", content: "dev note" } }),
	entry("j", "i", 8, { type: "message", message: { role: "bashExecution", command: "pwd", exitCode: 2 } }),
	entry("l", "j", 9, { type: "custom_message", customType: "irc:incoming", details: { from: "Peer", message: "hi back" } }),
	entry("m", "l", 10, { type: "compaction", summary: "<summary>\ncompacted" }),
	entry("n", "m", 11, { type: "custom", customType: "session_exit", data: { kind: "quit", reason: "done" } }),
	// Abandoned branch off "c": its prompt must not count as a turn, its result for k4 still completes the call.
	entry("x1", "c", 12, { type: "message", message: { role: "user", content: "abandoned prompt" } }),
	entry("x2", "x1", 13, result("k4", "read", [{ type: "text", text: "a".repeat(SPEECH + 5) }, { type: "image" }], true)),
	entry("o", "n", 14, { type: "label", label: "keep", targetId: "c" }),
	entry("p", "o", 15, { type: "message", message: { role: "user", content: "second prompt" } }),
];

/** The fields LIGHT keeps exact (see Build). */
function structure(event: SessionEvent): unknown {
	const base = { kind: event.kind, entry: event.entry, block: event.block, t: event.t, line: event.line, turn: event.turn };
	switch (event.kind) {
		case "call":
		case "dispatch":
			return { ...base, call: { ...event.call, args: null } };
		case "irc_out":
			return { ...base, to: event.to, call: { ...event.call, args: null } };
		case "irc_in":
			return { ...base, from: event.from, to: event.to };
		case "subresult":
			return { ...base, id: event.id, agent: event.agent, status: event.status, duration: event.duration };
		case "exec":
			return { ...base, lang: event.lang, status: event.status };
		case "marker":
			return { ...base, what: event.what };
		case "user":
			return { ...base, injected: event.injected };
		case "developer":
		case "thinking":
		case "say":
			return base;
		default: {
			const unreachable: never = event;
			throw new Error(`Unhandled event ${String(unreachable)}`);
		}
	}
}

describe("session-urls session loading", () => {
	test("tool output is measured to contentText's length for every content shape", async () => {
		const shapes: unknown[] = [
			"plain",
			"",
			[],
			[{ type: "text", text: "one" }],
			[{ type: "text", text: "a" }, { type: "image" }, { type: "text", text: "b\nc" }, "stray", null, { type: "text", text: 5 }],
			[{ type: "image" }, { type: "image", text: "caption" }],
			[{ type: "text", text: 'esc \\ " \t \u0001 é ✓ 😀' }],
			[{ type: "text", text: "x".repeat(SPEECH) }, { type: "image" }, { type: "text", text: "y" }],
			[{ type: "text", text: "x".repeat(SPEECH + 1) }, { type: "image" }, { type: "text", text: "y" }],
			Array.from({ length: 1003 }, () => ({ type: "text", text: "zz" })),
			{ type: "text", text: "not an array" },
			42,
		];
		const records: unknown[] = [HEADER];
		const blocks = shapes.map((_, index) => call(`c${index}`, "read", {}));
		records.push(entry("asst", null, 0, { type: "message", message: { role: "assistant", content: blocks } }));
		let parent = "asst";
		shapes.forEach((content, index) => {
			records.push(entry(`r${index}`, parent, 1, result(`c${index}`, "read", content)));
			parent = `r${index}`;
		});
		const loaded = await loadSession(sessionFile(records), "sid", "");
		const chars = loaded.events.flatMap((event) => (event.kind === "call" ? [event.call.resultChars] : []));
		expect(chars).toEqual(shapes.map(expectedChars));
	});

	test("branch, turns, call completion and subagent results", async () => {
		const loaded = await loadSession(sessionFile(MIXED), "sid", "");
		expect(loaded.meta).toMatchObject({ leaf: "p", entries: MIXED.length - 2, onBranch: 16, title: "Mixed" });
		const shown = loaded.events.map(
			(event) => `${event.turn} ${event.kind} ${event.entry}${event.block === null ? "" : `.${event.block}`}`,
		);
		expect(shown).toEqual([
			"0 marker a",
			"1 user b",
			"1 thinking c.0",
			"1 marker c",
			"1 say c.3",
			"1 call c.4",
			"1 irc_out c.5",
			"1 dispatch c.6",
			"1 call c.7",
			"1 subresult f",
			"1 user h",
			"1 developer i",
			"1 exec j",
			"1 irc_in l",
			"1 marker m",
			"1 marker n",
			"1 marker o",
			"2 user p",
		]);
		const calls = Object.fromEntries(
			loaded.events.flatMap((event) =>
				event.kind === "call" || event.kind === "irc_out" || event.kind === "dispatch" ? [[event.call.id, event.call]] : [],
			),
		);
		expect(calls.k1).toMatchObject({ intent: "Started", status: "ok", resultEntry: "d", resultChars: expectedChars(K1_OUTPUT) });
		expect(calls.k4).toMatchObject({ status: "error", resultEntry: "x2", resultChars: SPEECH + 5 });
		expect(loaded.events.find((event) => event.kind === "user")).toMatchObject({ text: "first prompt with session://sid/artifacts/3" });
	});

	test("a subagent's attributed prompts open turns", async () => {
		const loaded = await loadSession(sessionFile(MIXED), "sid", "Kid");
		expect(loaded.events.filter((event) => event.kind === "user").map((event) => event.turn)).toEqual([1, 2, 3]);
	});

	test("LIGHT keeps FULL's structure; skipped entries are not built but still deduplicate subagent results", async () => {
		const file = sessionFile(MIXED);
		const index = await indexSession(file, "sid", "");
		const full = await buildEvents(index, () => "full");
		const light = await buildEvents(index, () => "light");
		expect(light.map(structure)).toEqual(full.map(structure));
		expect(JSON.stringify(light)).not.toContain("say it");

		const only =
			(entries: string[]): ((at: { entry: string }) => Build) =>
			(at) =>
				entries.includes(at.entry) ? "full" : "skip";
		const late = await buildEvents(index, only(["g", "p"]));
		expect(late.map((event) => `${event.kind} ${event.entry}`)).toEqual(["user p"]);
		const withFirst = await buildEvents(index, only(["f", "g"]));
		expect(withFirst.map((event) => `${event.kind} ${event.entry}`)).toEqual(["subresult f"]);
		expect(await buildEvents(index, only(["c"]))).toEqual(full.filter((event) => event.entry === "c"));
	});
});
