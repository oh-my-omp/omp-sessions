// Running caller SQL on a session database, and exporting a query result.
// Statements are parsed by DuckDB and admitted by statement type: queries and
// changes to the database's own tables, macros and views. Statements that
// configure the engine or reach files (COPY, EXPORT, ATTACH, INSTALL, LOAD,
// SET, PRAGMA, CALL, …) are refused; export writes its file from a separate
// engine that runs no caller SQL.
import { realpathSync } from "node:fs";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { type DuckDBConnection, DuckDBInstance, type Json, JsonDuckDBValueConverter, StatementType } from "@duckdb/node-api";
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

/**
 * Characters one `sql` result may spend: `limit` bounds the row count, the
 * budget bounds what those rows hold, so a single huge cell (one transcript
 * line can be 200 MB) never reaches the caller. Counted per value without
 * serializing a row: a string by its length, a number, boolean or null by its
 * JSON text, and a list or struct by its values recursively.
 */
const RESULT_CHAR_BUDGET = 16 * 1024 * 1024;

function valueChars(value: Json): number {
	if (value === null) return 4;
	if (typeof value === "string") return value.length;
	if (typeof value === "number") return String(value).length;
	if (typeof value === "boolean") return value ? 4 : 5;
	let chars = 0;
	if (Array.isArray(value)) {
		for (const item of value) chars += valueChars(item);
		return chars;
	}
	for (const item of Object.values(value)) chars += valueChars(item);
	return chars;
}

function statementName(type: StatementType): string {
	return StatementType[type] ?? String(type);
}

/**
 * Runs every statement of `query` in order and returns the last one's result:
 * at most `limit` rows when it returns rows, within the character budget, else
 * the number of rows it changed. `truncated` reports that a row was not
 * returned. `params` bind the only statement; a script with parameters is refused.
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
		// Chunks are read one at a time and converted one row at a time, so a
		// result is never materialized whole: the previous shape fetched every
		// row as JSON and then copied the array with slice.
		const result = await prepared.stream();
		try {
			const columns = result.columnNames();
			const names = result.deduplicatedColumnNames();
			const rows: Record<string, Json>[] = [];
			let chars = 0;
			let truncated = false;
			let chunk = await result.fetchChunk();
			while (chunk !== null && chunk.rowCount > 0 && !truncated) {
				const rowCount = chunk.rowCount;
				for (let rowIndex = 0; rowIndex < rowCount; rowIndex++) {
					if (rows.length === limit) {
						truncated = true;
						break;
					}
					const row: Record<string, Json> = {};
					let rowChars = 0;
					chunk.visitRowValues(rowIndex, (value, _rowIndex, columnIndex, type) => {
						const converted = JsonDuckDBValueConverter(value, type, JsonDuckDBValueConverter);
						rowChars += valueChars(converted);
						const name = names[columnIndex];
						if (name !== undefined) row[name] = converted;
					});
					if (chars + rowChars > RESULT_CHAR_BUDGET) {
						truncated = true;
						break;
					}
					rows.push(row);
					chars += rowChars;
				}
				if (!truncated) chunk = await result.fetchChunk();
			}
			return { kind: "rows", columns, rows, truncated };
		} finally {
			result.closeSync();
		}
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

/** The real path of an existing file or directory, or null when there is none. */
function realPath(target: string): string | null {
	try {
		return realpathSync.native(target);
	} catch {
		// ENOENT or ENOTDIR: nothing exists at the path.
		return null;
	}
}

/**
 * An absolute export path outside the sessions directory, whose directory exists.
 * Both sides are compared as real paths, so another spelling of the sessions
 * directory (case, 8.3 short names, symbolic links) still lies inside it. omp's
 * `pathIsWithin` (`@oh-my-pi/pi-utils`) is not used: it resolves with
 * `fs.realpathSync`, which on Windows keeps 8.3 short names and junctions as written.
 */
async function exportPath(target: string): Promise<string> {
	if (!path.isAbsolute(target)) throw new Error(`export path ${target} is not absolute`);
	const resolved = path.resolve(target);
	const directory = path.dirname(resolved);
	const stat = await fs.stat(directory).catch(() => null);
	if (!stat?.isDirectory()) throw new Error(`export directory ${directory} does not exist`);
	const real = realPath(resolved) ?? path.join(realPath(directory) ?? directory, path.basename(resolved));
	const sessions = getSessionsDir();
	const relative = path.relative(realPath(sessions) ?? path.resolve(sessions), real);
	if (relative === "" || (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative)))
		throw new Error("export path lies inside the omp sessions directory");
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
