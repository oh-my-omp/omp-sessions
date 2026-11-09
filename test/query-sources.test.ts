// Schema `src` exists only for what a call's SQL reaches: the `src.<name>` its
// parse tree names, the definitions of the views and macros the database keeps,
// and every relation when that tree names none (a table function, a `SHOW`, a
// statement DuckDB cannot serialize, a parse error). `data` in `src.events`
// holds the record without the field its text column already carries.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { type DuckDBConnection, DuckDBInstance } from "@duckdb/node-api";
import { SessionDatabases } from "../src/query/database.ts";
import { runSql } from "../src/query/run.ts";
import { eventRow, referencedRelations } from "../src/query/sources.ts";
import { sessionQueryTool } from "../src/query/tool.ts";
import type { EventRecord, TranscriptRow } from "../src/relations.ts";

/** The answer of one `referencedRelations` call, flattened for comparison. */
function flatten(referenced: "all" | ReadonlySet<string>): string[] {
	return referenced === "all" ? ["all"] : [...referenced].sort();
}

describe("referencedRelations", () => {
	let instance: DuckDBInstance;
	let connection: DuckDBConnection;
	beforeAll(async () => {
		instance = await DuckDBInstance.create(":memory:");
		connection = await instance.connect();
	});
	afterAll(() => {
		connection.closeSync();
		instance.closeSync();
	});

	test("names each relation a query qualifies with src", async () => {
		for (const name of ["files", "entries", "sessions", "transcripts", "events"])
			expect(flatten(await referencedRelations(connection, `SELECT * FROM src.${name}`, []))).toEqual([name]);
	});

	test("reads the parse tree, so case, quotes, comments and spacing do not matter", async () => {
		expect(flatten(await referencedRelations(connection, 'SELECT * FROM "SRC" . /* c */ "Entries"', []))).toEqual(["entries"]);
		expect(flatten(await referencedRelations(connection, "SELECT * FROM Src\t.\tEvents", []))).toEqual(["events"]);
		expect(flatten(await referencedRelations(connection, "SELECT * FROM src/**/.files", []))).toEqual(["files"]);
		expect(flatten(await referencedRelations(connection, "SELECT count(*) FROM src . FILES", []))).toEqual(["files"]);
		expect(flatten(await referencedRelations(connection, "SELECT * FROM src.files UNION ALL SELECT * FROM src.transcripts", []))).toEqual([
			"files",
			"transcripts",
		]);
		expect(
			flatten(await referencedRelations(connection, "SELECT * FROM src.events WHERE kind IN (SELECT kind FROM src.entries)", [])),
		).toEqual(["entries", "events"]);
		expect(flatten(await referencedRelations(connection, "WITH c AS (SELECT * FROM src.events) SELECT * FROM c", []))).toEqual(["events"]);
	});

	test("builds nothing when the tree names no src relation", async () => {
		expect(flatten(await referencedRelations(connection, "SELECT 1", []))).toEqual([]);
		expect(flatten(await referencedRelations(connection, "SELECT * FROM main.files", []))).toEqual([]);
	});

	test("builds every relation for run-time names and statements without a tree", async () => {
		const queries = [
			"SELECT * FROM duckdb_tables()",
			"SELECT * FROM duckdb_columns()",
			"SELECT * FROM duckdb_views()",
			"SELECT * FROM information_schema.tables",
			"SELECT * FROM pg_class",
			"SELECT * FROM pg_catalog.pg_namespace",
			"SELECT * FROM sqlite_master",
			"SHOW TABLES",
			"SHOW ALL TABLES",
			"DESCRIBE src.entries",
			"SUMMARIZE SELECT * FROM src.events",
			"SELECT * FROM query('SELECT 1')",
			"SELECT * FROM query_table('src.events')",
			'SELECT * FROM "query_table"(?)',
			"SELECT * FROM pragma_table_info(?)",
			"CREATE TABLE t AS SELECT 42 AS n",
			"SELECT x FROM src.files WHERE (",
		];
		for (const query of queries) expect(flatten(await referencedRelations(connection, query, []))).toEqual(["all"]);
	});

	test("builds every relation when a stored definition can reach src", async () => {
		expect(flatten(await referencedRelations(connection, "SELECT 1", ["CREATE VIEW v AS SELECT * FROM src.files"]))).toEqual(["all"]);
		expect(flatten(await referencedRelations(connection, "SELECT 1", ["CREATE VIEW v AS SELECT * FROM main.other"]))).toEqual([]);
	});
});

const transcript: TranscriptRow = {
	sid: "s",
	agent: "",
	parent: null,
	file: "/tmp/sessions/s.jsonl",
	bytes: 1,
	modified: "2026-01-01T00:00:00.000Z",
};

/** The cells `data` and `text` of one event row. */
function row(record: EventRecord): { text: unknown; data: unknown } {
	const cells = eventRow(transcript, 0, record);
	return { text: cells[13], data: cells[14] };
}

describe("eventRow", () => {
	test("data keeps the record without the text its own column copies", () => {
		const { text, data } = row({ kind: "say", text: "hello", url: "u" });
		expect(text).toBe("hello");
		expect(JSON.parse(String(data))).toEqual({ kind: "say", url: "u" });
	});

	test("source fills the text column when the record has no text", () => {
		const { text, data } = row({ kind: "exec", source: "ls", url: "u" });
		expect(text).toBe("ls");
		expect(JSON.parse(String(data))).toEqual({ kind: "exec", url: "u" });
	});

	test("text wins over source, and data keeps source", () => {
		const { text, data } = row({ text: "shown", source: "kept" });
		expect(text).toBe("shown");
		expect(JSON.parse(String(data))).toEqual({ source: "kept" });
	});

	test("a record with neither keeps its whole JSON", () => {
		const { text, data } = row({ kind: "marker", what: "compaction" });
		expect(text).toBeNull();
		expect(JSON.parse(String(data))).toEqual({ kind: "marker", what: "compaction" });
	});
});

describe("session_query admission", () => {
	test("USE and SET cannot put src on the search path", async () => {
		const instance = await DuckDBInstance.create(":memory:");
		const connection = await instance.connect();
		try {
			await connection.run("CREATE SCHEMA src");
			for (const query of ["USE src", "SET search_path='src'", "SET schema='src'"])
				await expect(runSql(connection, query, null, 10)).rejects.toThrow("is SET;");
		} finally {
			connection.closeSync();
			instance.closeSync();
		}
	});

	test("an undeclared key fails a sessions call and a sql call, and the error names it", async () => {
		const databases = new SessionDatabases();
		try {
			const tool = sessionQueryTool(databases);
			// The host passes raw arguments; ctx is read only after parsing succeeds, so it is not needed here.
			const calls = [
				{ action: "sessions", bogus: 1 },
				{ action: "sql", query: "select 1", bogus: 1 },
			];
			for (const args of calls) {
				const result = await tool.execute("admission", args as never, undefined, undefined, undefined as never);
				const text = result.content.map((part) => ("text" in part ? part.text : "")).join("");
				expect(result.isError).toBe(true);
				expect(text).toContain("bogus");
			}
		} finally {
			await databases.close();
		}
	});
});
