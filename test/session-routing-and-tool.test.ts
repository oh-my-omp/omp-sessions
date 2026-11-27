import { afterEach, describe, expect, test } from "bun:test";
import type { InternalUrl } from "@oh-my-pi/pi-coding-agent/internal-urls";
import { clearToolTopics, registerToolTopics, resolveToolTopic } from "../omomp-shared/internal-url.ts";
import { omompTool } from "../omomp-shared/tool-entry.ts";
import { QueryProcesses } from "../src/engine/query.ts";
import { routeSpecialRead, SessionUrlHandler } from "../src/handler.ts";
import { approval, DESCRIPTION, parameters, parseRequest, sessionQuerySqlTopic, sessionQueryTool } from "../src/query/tool.ts";

function fakeUrl(href: string): InternalUrl {
	const parsed = new URL(href);
	return {
		href,
		scheme: parsed.protocol.replace(/:$/, ""),
		hostname: parsed.hostname,
		rawHost: parsed.hostname,
		pathname: parsed.pathname,
		rawPathname: parsed.pathname,
		search: parsed.search,
		searchParams: parsed.searchParams,
	} as unknown as InternalUrl;
}

const noopLogger = {
	debug() {},
	info() {},
	warn() {},
	error() {},
} as unknown as ConstructorParameters<typeof SessionUrlHandler>[0];

afterEach(() => {
	clearToolTopics("omp-sessions");
});

describe("session:// /agents and /events routing", () => {
	test("routes session://<sid>/agents and session://here/agents", () => {
		expect(routeSpecialRead(fakeUrl("session://019a1234/agents"), undefined)).toEqual({
			op: "agents",
			sid: "019a1234",
		});
		expect(routeSpecialRead(fakeUrl("session://here/agents"), { sessionId: "cur-sid" })).toEqual({
			op: "agents",
			sid: "cur-sid",
		});
		expect(
			routeSpecialRead(fakeUrl("session://here/agents"), {
				sessionFile: "/tmp/2026-11-22T00-00-00Z_abc-def-123.jsonl",
			}),
		).toEqual({
			op: "agents",
			sid: "abc-def-123",
		});
	});

	test("routes session://<sid>/events and session://here/events with filters and paging", () => {
		expect(routeSpecialRead(fakeUrl("session://019a1234/events?type=thinking&type=irc&agent=A1&limit=100&offset=20"), undefined)).toEqual({
			op: "events",
			sid: "019a1234",
			type: "thinking,irc",
			agent: "A1",
			limit: 100,
			offset: 20,
		});
		expect(routeSpecialRead(fakeUrl("session://here/events?limit=50"), { sessionId: "cur-sid" })).toEqual({
			op: "events",
			sid: "cur-sid",
			limit: 50,
		});
	});

	test("leaves standard routes and directory walks to the engine", () => {
		expect(routeSpecialRead(fakeUrl("session://019a1234/timeline"), undefined)).toBeNull();
		expect(routeSpecialRead(fakeUrl("session://here/2026-11-22/019a1234/timeline"), undefined)).toBeNull();
		expect(routeSpecialRead(fakeUrl("session://019a1234/agents"), { skipDirectoryListing: true })).toBeNull();
		expect(routeSpecialRead(fakeUrl("session://019a1234/agents/"), undefined)).toBeNull();
	});

	test("rejects invalid /agents and /events query parameters or missing active sessionId", () => {
		expect(() => routeSpecialRead(fakeUrl("session://here/agents"), {})).toThrow(/sessionId/);
		expect(() => routeSpecialRead(fakeUrl("session://019a1234/agents?limit=10"), undefined)).toThrow(/query/);
		expect(() => routeSpecialRead(fakeUrl("session://019a1234/events?bad=1"), undefined)).toThrow(/unknown query parameter/);
		expect(() => routeSpecialRead(fakeUrl("session://019a1234/events?limit=5001"), undefined)).toThrow(/1\.\.5000/);
	});

	test("promptDoc and url-grammar document /agents and /events", () => {
		const handler = new SessionUrlHandler(noopLogger);
		expect(handler.promptDoc()).toContain("/agents");
		expect(handler.promptDoc()).toContain("/events");
		const ref = handler.promptReference();
		expect(ref).toContain("session://<id>/agents");
		expect(ref).toContain("session://here/agents");
		expect(ref).toContain("session://<id>/events");
		expect(ref).toContain("session://here/events");
	});
});

describe("session_query refactored schema and topic", () => {
	test("description is <= 200 chars and points to help://session_query/sql", () => {
		expect(DESCRIPTION.length).toBeLessThanOrEqual(200);
		expect(DESCRIPTION).toContain("help://session_query/sql");
	});

	test("accepts native JSON tables, tagged params, scope, and export_to", () => {
		const parsed = parseRequest({
			query: "SELECT * FROM src.events WHERE kind = $kind",
			from: "here",
			sids: ["019a"],
			agents: ["AgentA"],
			agent_scope: "root_only",
			tables: [{ name: "custom_events", sids: ["019a"], agent: "AgentA" }],
			params: {
				bindings: [
					{ key: "kind", kind: "text", text: "call" },
					{ key: "limit", kind: "number", number: 10 },
					{ key: "ok", kind: "boolean", boolean: true },
					{ key: "empty", kind: "null" },
				],
			},
			export_to: { path: "/tmp/out.parquet", format: "parquet" },
		});
		expect(parsed.export_to).toEqual({ path: "/tmp/out.parquet", format: "parquet" });
		expect(parsed.params).toEqual({
			bindings: [
				{ key: "kind", kind: "text", value: "call" },
				{ key: "limit", kind: "number", value: 10 },
				{ key: "ok", kind: "boolean", value: true },
				{ key: "empty", kind: "null" },
			],
		});
		expect(parsed.agent_scope).toBe("root_only");
		expect(parsed.tables).toEqual([{ name: "custom_events", sids: ["019a"], agent: "AgentA" }]);
	});

	test("preserves explicit SQL NULL and empty-string bindings", () => {
		const parsed = parseRequest({
			query: "SELECT $missing IS NULL, $empty = ''",
			params: {
				bindings: [
					{ key: "missing", kind: "null" },
					{ key: "empty", kind: "empty_text" },
					{ key: "zero", kind: "number", number: 0 },
					{ key: "false", kind: "boolean", boolean: false },
				],
			},
		});
		expect(parsed.params).toEqual({
			bindings: [
				{ key: "missing", kind: "null" },
				{ key: "empty", kind: "empty_text" },
				{ key: "zero", kind: "number", value: 0 },
				{ key: "false", kind: "boolean", value: false },
			],
		});
	});

	test("through the shared entry, empty text is omitted while empty_text, null and spaced text survive", async () => {
		let received: unknown;
		const tool = omompTool({
			name: "session_query",
			parameters,
			async execute(_id: string, args: unknown) {
				received = args;
				return { content: [] };
			},
		});
		await tool.execute("probe", {
			query: "SELECT $a, $b, $c, $d",
			agents: [],
			params: {
				bindings: [
					{ key: "a", kind: "null" },
					{ key: "b", kind: "empty_text" },
					{ key: "c", kind: "text", text: "  " },
					{ key: "d", kind: "text", text: "" },
				],
			},
		});
		expect(received).toEqual({
			query: "SELECT $a, $b, $c, $d",
			params: {
				bindings: [
					{ key: "a", kind: "null" },
					{ key: "b", kind: "empty_text" },
					{ key: "c", kind: "text", text: "  " },
					{ key: "d", kind: "text" },
				],
			},
		});
		expect(() => parseRequest(received)).toThrow("kind text requires only text");
	});

	test("rejects legacy action field and stringified params/tables", () => {
		expect(() => parseRequest({ action: "sql", query: "SELECT 1" })).toThrow();
		expect(() => parseRequest({ query: "SELECT 1", params: '{"a":1}' })).toThrow();
		expect(() => parseRequest({ query: "SELECT 1", tables: '{"t":[{"a":1}]}' })).toThrow();
	});

	test("approval is read for queries and write for export_to", () => {
		expect(approval({ query: "SELECT 1" })).toBe("read");
		expect(approval({ query: "SELECT 1", export_to: { path: "/tmp/out.csv", format: "csv" } })).toBe("write");
	});

	test("registers help://session_query/sql via registerToolTopics and native docTopics", () => {
		registerToolTopics("omp-sessions", [sessionQuerySqlTopic()]);
		const resolved = resolveToolTopic("session_query", "sql");
		expect(resolved).toBeDefined();
		expect(resolved?.content).toContain("export_to");
		expect(resolved?.content).not.toContain('action: "sql"');
		expect(resolveToolTopic("session_query", "actions")).toBeUndefined();
		const tool = sessionQueryTool(new QueryProcesses(noopLogger), noopLogger);
		expect(tool.docTopics().sql).toContain("export_to");
	});
});
