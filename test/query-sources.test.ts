// Schema `src` exists only for what a call's SQL reaches: the `src.<name>` it
// writes, the definitions of the views and macros the database keeps, and every
// relation when the text goes through the catalog or resolves a name at run
// time. `data` in `src.events` holds the record without the field its text
// column already carries.
import { describe, expect, test } from "bun:test";
import { DuckDBInstance } from "@duckdb/node-api";
import { runSql } from "../src/query/run.ts";
import { eventRow, referencedRelations } from "../src/query/sources.ts";
import type { EventRecord, TranscriptRow } from "../src/relations.ts";

function names(...queries: string[]): string[] {
	const referenced = referencedRelations(queries);
	return referenced === "all" ? ["all"] : [...referenced].sort();
}

describe("referencedRelations", () => {
	test("names each relation a query qualifies with src", () => {
		for (const name of ["files", "entries", "sessions", "transcripts", "events"])
			expect(names(`SELECT * FROM src.${name}`)).toEqual([name]);
	});

	test("ignores case, quotes and whitespace", () => {
		expect(names('SELECT * FROM "SRC" . "Entries"')).toEqual(["entries"]);
		expect(names("SELECT * FROM Src\t.\tEvents")).toEqual(["events"]);
		expect(names("select count(*) from src . FILES", "select * from src.transcripts")).toEqual(["files", "transcripts"]);
	});

	test("builds nothing when the text names no relation", () => {
		expect(names("SELECT 1", "CREATE TABLE t AS SELECT 42 AS n")).toEqual([]);
	});

	test("builds every relation for catalog and run-time names", () => {
		const queries = [
			"SELECT * FROM duckdb_tables()",
			"SELECT * FROM duckdb_columns()",
			"SELECT * FROM duckdb_views()",
			"SELECT * FROM information_schema.tables",
			"SELECT * FROM pg_class",
			"SELECT * FROM pg_catalog.pg_namespace",
			"SELECT * FROM sqlite_master",
			"SHOW TABLES",
			"DESCRIBE src.entries",
			"SUMMARIZE SELECT * FROM src.events",
			"SELECT * FROM query('SELECT 1')",
			"SELECT * FROM query_table('src.events')",
		];
		for (const query of queries) expect(names(query)).toEqual(["all"]);
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
});
