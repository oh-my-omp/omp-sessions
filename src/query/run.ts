// Running caller SQL on a session database, and exporting a query result.
// Statements are parsed by DuckDB and admitted by statement type: queries and
// changes to the database's own tables, macros and views. Statements that
// configure the engine or reach files (COPY, EXPORT, ATTACH, INSTALL, LOAD,
// SET, PRAGMA, CALL, …) are refused; export writes its file from a separate
// engine that runs no caller SQL.
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { type DuckDBConnection, DuckDBInstance, type Json, StatementType } from "@duckdb/node-api";
import { getSessionsDir } from "../omp-adapter.ts";
import { identifier, literal } from "./database.ts";

const ADMITTED: ReadonlySet<StatementType> = new Set([
	StatementType.SELECT,
	StatementType.INSERT,
	StatementType.UPDATE,
	StatementType.DELETE,
	StatementType.MERGE_INTO,
	StatementType.CREATE,
	StatementType.CREATE_FUNC,
	StatementType.ALTER,
	StatementType.DROP,
	StatementType.EXPLAIN,
]);

export type Bindings = (string | number | boolean | null)[] | Record<string, string | number | boolean | null>;

export type SqlResult =
	| { readonly kind: "rows"; readonly columns: readonly string[]; readonly rows: Record<string, Json>[]; readonly truncated: boolean }
	| { readonly kind: "changes"; readonly changes: number };

function statementName(type: StatementType): string {
	return StatementType[type] ?? String(type);
}

/**
 * Runs every statement of `query` in order and returns the last one's result:
 * at most `limit` rows when it returns rows, else the number of rows it changed.
 * `params` bind the only statement; a script with parameters is refused.
 */
export async function runSql(connection: DuckDBConnection, query: string, params: Bindings | null, limit: number): Promise<SqlResult> {
	const extracted = await connection.extractStatements(query);
	if (params !== null && extracted.count > 1) throw new Error("params bind a single statement; this query holds several");
	for (let index = 0; index < extracted.count; index++) {
		const prepared = await extracted.prepare(index);
		if (!ADMITTED.has(prepared.statementType))
			throw new Error(
				`statement ${index + 1} is ${statementName(prepared.statementType)}; session_query runs queries and changes to its own tables, views and macros only (use action "export" to write a file)`,
			);
		if (params !== null) prepared.bind(params);
		if (index < extracted.count - 1) {
			await prepared.run();
			continue;
		}
		if (prepared.columnCount === 0) return { kind: "changes", changes: (await prepared.run()).rowsChanged };
		const reader = await prepared.streamAndReadUntil(limit + 1);
		const rows = reader.getRowObjectsJson();
		reader.closeSync();
		return { kind: "rows", columns: reader.columnNames(), rows: rows.slice(0, limit), truncated: rows.length > limit };
	}
	throw new Error("query holds no statement");
}

export type ExportFormat = "parquet" | "csv" | "json";

export interface ExportResult {
	readonly path: string;
	readonly format: ExportFormat;
	readonly rows: number;
	readonly columns: readonly string[];
}

/** An absolute export path outside the sessions directory, whose directory exists. */
async function exportPath(target: string): Promise<string> {
	if (!path.isAbsolute(target)) throw new Error(`export path ${target} is not absolute`);
	const resolved = path.resolve(target);
	const sessions = path.resolve(getSessionsDir());
	if (resolved === sessions || resolved.startsWith(`${sessions}${path.sep}`))
		throw new Error("export path lies inside the omp sessions directory");
	const stat = await fs.stat(path.dirname(resolved)).catch(() => null);
	if (!stat?.isDirectory()) throw new Error(`export directory ${path.dirname(resolved)} does not exist`);
	return resolved;
}

/** Runs `query` (one row-returning statement) and writes its whole result to `target`. */
export async function exportResult(
	connection: DuckDBConnection,
	query: string,
	params: Bindings | null,
	target: string,
	format: ExportFormat,
): Promise<ExportResult> {
	const file = await exportPath(target);
	const extracted = await connection.extractStatements(query);
	if (extracted.count !== 1) throw new Error("export takes one statement that returns rows");
	const prepared = await extracted.prepare(0);
	if (prepared.statementType !== StatementType.SELECT)
		throw new Error(`export takes a SELECT, not ${statementName(prepared.statementType)}`);
	if (params !== null) prepared.bind(params);
	const result = await prepared.stream();
	const columns = result.columnNames();
	const types = result.columnTypes();
	const writer = await DuckDBInstance.create(":memory:");
	const out = await writer.connect();
	try {
		await out.run(`CREATE TABLE result (${columns.map((name, index) => `${identifier(name)} ${String(types[index])}`).join(", ")})`);
		const appender = await out.createAppender("result");
		let rows = 0;
		try {
			for await (const chunk of result) {
				appender.appendDataChunk(chunk);
				rows += chunk.rowCount;
			}
		} finally {
			appender.closeSync();
		}
		await out.run(`COPY result TO ${literal(file)} (FORMAT ${format})`);
		return { path: file, format, rows, columns };
	} finally {
		out.closeSync();
		writer.closeSync();
	}
}
