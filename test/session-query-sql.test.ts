// session_query `sql`: runSql returns the last statement's rows as JSON, shuts
// off at the row limit and at the character budget, and reports truncation;
// statements that change rows come back as their count.
import { afterAll, describe, expect, test } from "bun:test";
import { DuckDBInstance } from "@duckdb/node-api";
import { runSql } from "../src/query/run.ts";

const instance = await DuckDBInstance.create(":memory:");
const connection = await instance.connect();
afterAll(() => {
	connection.closeSync();
	instance.closeSync();
});

describe("runSql rows", () => {
	test("converts every column type to the same JSON getRowObjectsJson produced", async () => {
		const result = await runSql(
			connection,
			`SELECT 'text' AS v, 9223372036854775807 AS big, 1.5 AS dec, CAST(1.5 AS DOUBLE) AS dbl,
				CAST('Infinity' AS DOUBLE) AS inf, true AS b, NULL AS n, CAST('{"a":[1,2]}' AS JSON) AS j,
				[1, 2] AS list, {'x': 1} AS struct, MAP {'k': 1} AS map`,
			null,
			10,
		);
		expect(result).toEqual({
			kind: "rows",
			columns: ["v", "big", "dec", "dbl", "inf", "b", "n", "j", "list", "struct", "map"],
			rows: [
				{
					v: "text",
					big: "9223372036854775807",
					dec: "1.5",
					dbl: 1.5,
					inf: "Infinity",
					b: true,
					n: null,
					j: '{"a":[1,2]}',
					list: [1, 2],
					struct: { x: 1 },
					map: [{ key: "k", value: 1 }],
				},
			],
			truncated: false,
		});
	});

	test("numbers duplicate column names as getRowObjectsJson did", async () => {
		expect(await runSql(connection, "SELECT 1 AS a, 2 AS a, 3 AS b", null, 10)).toEqual({
			kind: "rows",
			columns: ["a", "a", "b"],
			rows: [{ a: 1, "a:1": 2, b: 3 }],
			truncated: false,
		});
	});

	test("an empty result has no rows and is not truncated", async () => {
		expect(await runSql(connection, "SELECT 1 AS a WHERE false", null, 10)).toEqual({
			kind: "rows",
			columns: ["a"],
			rows: [],
			truncated: false,
		});
	});

	test("stops at the row limit and reports only a row that exists", async () => {
		const capped = await runSql(connection, "SELECT i FROM range(3) t(i)", null, 2);
		expect(capped).toEqual({ kind: "rows", columns: ["i"], rows: [{ i: "0" }, { i: "1" }], truncated: true });
		const whole = await runSql(connection, "SELECT i FROM range(2) t(i)", null, 2);
		expect(whole).toEqual({ kind: "rows", columns: ["i"], rows: [{ i: "0" }, { i: "1" }], truncated: false });
	});
});

describe("runSql character budget", () => {
	const megabyte = 1024 * 1024;

	test("returns the rows that fit 16 MiB and holds back the one that does not", async () => {
		// Two 8 MiB values fill the budget exactly; the third row does not fit.
		const result = await runSql(connection, `SELECT repeat('x', ${8 * megabyte}) AS v FROM range(3)`, null, 100);
		expect(result.kind).toBe("rows");
		if (result.kind !== "rows") return;
		expect(result.rows.length).toBe(2);
		expect(result.rows.map((row) => (typeof row.v === "string" ? row.v.length : 0))).toEqual([8 * megabyte, 8 * megabyte]);
		expect(result.truncated).toBe(true);
	});

	test("a single value over the whole budget returns no rows and says so", async () => {
		const result = await runSql(connection, `SELECT repeat('x', ${17 * megabyte}) AS v`, null, 100);
		expect(result).toEqual({ kind: "rows", columns: ["v"], rows: [], truncated: true });
	});

	test("counts nested values by their contents", async () => {
		const result = await runSql(connection, `SELECT [repeat('x', ${9 * megabyte})] AS v FROM range(2)`, null, 100);
		expect(result.kind).toBe("rows");
		if (result.kind !== "rows") return;
		expect(result.rows.length).toBe(1);
		expect(result.truncated).toBe(true);
	});
});

describe("runSql statement results", () => {
	test("a changing statement returns the Count column DuckDB produces", async () => {
		await runSql(connection, "CREATE TABLE changes (n INTEGER)", null, 10);
		expect(await runSql(connection, "INSERT INTO changes VALUES (1), (2)", null, 10)).toEqual({
			kind: "rows",
			columns: ["Count"],
			rows: [{ Count: "2" }],
			truncated: false,
		});
	});

	test("a script returns the last statement's result", async () => {
		expect(await runSql(connection, "INSERT INTO changes VALUES (3); SELECT count(*) AS n FROM changes", null, 10)).toEqual({
			kind: "rows",
			columns: ["n"],
			rows: [{ n: "3" }],
			truncated: false,
		});
	});
});
