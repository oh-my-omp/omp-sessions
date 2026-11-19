import type { DuckDBAppender, DuckDBConnection } from "@duckdb/node-api";
import { readSessionInfo } from "../omp-adapter.ts";
import { type EventRecord, metadata, type TranscriptRow, transcriptEvents, transcriptTree, transcriptUrl } from "../relations.ts";
import { locateSession, type SessionRef } from "../routes.ts";
import { identifier, literal } from "./database.ts";

export const SOURCE_RELATIONS: readonly string[] = ["files", "entries", "sessions", "transcripts", "events"];

const DEFINITION_REACHES_SRC =
	/\b(?:src|query|query_table|pragma_\w+|duckdb_\w+|pg_\w+|sqlite_\w+|information_schema|show|describe|summarize)\b/i;

const CATALOG_SCHEMA = /^(?:information_schema|pg_catalog)$/i;
const CATALOG_RELATION = /^(?:duckdb|pg|sqlite)_\w+$/i;

async function parseReferences(connection: DuckDBConnection, sql: string): Promise<"all" | Set<string>> {
	let parsed: unknown;
	try {
		const result = await connection.run("SELECT json_serialize_sql(?)", [sql]);
		const value = (await result.getRows())[0]?.[0];
		parsed = typeof value === "string" ? JSON.parse(value) : null;
	} catch {
		return "all";
	}
	if (parsed === null || typeof parsed !== "object") return "all";
	const tree = parsed as { error?: unknown; statements?: unknown };
	if (tree.error === true || !Array.isArray(tree.statements)) return "all";
	const names = new Set<string>();
	const walk = (node: unknown): boolean => {
		if (Array.isArray(node)) {
			for (const item of node) if (!walk(item)) return false;
			return true;
		}
		if (node === null || typeof node !== "object") return true;
		const fields = node as Record<string, unknown>;
		if (fields.type === "TABLE_FUNCTION" || fields.type === "SHOW_REF") return false;
		if (fields.type === "BASE_TABLE") {
			const schema = typeof fields.schema_name === "string" ? fields.schema_name.toLowerCase() : "";
			const table = typeof fields.table_name === "string" ? fields.table_name.toLowerCase() : "";
			if (CATALOG_SCHEMA.test(schema) || CATALOG_RELATION.test(table)) return false;
			if (schema === "src" && SOURCE_RELATIONS.includes(table)) names.add(table);
		}
		for (const value of Object.values(fields)) if (!walk(value)) return false;
		return true;
	};
	for (const statement of tree.statements) if (!walk(statement)) return "all";
	return names;
}

/**
 * Building a relation nobody reads only
 * costs allocations; missing one the query reads fails it, so an uncertain
 * answer is `all`.
 */
export async function referencedRelations(
	connection: DuckDBConnection,
	sql: string,
	definitions: readonly string[],
): Promise<"all" | ReadonlySet<string>> {
	if (definitions.some((text) => DEFINITION_REACHES_SRC.test(text))) return "all";
	return parseReferences(connection, sql);
}

async function definitionTexts(connection: DuckDBConnection): Promise<string[]> {
	const definitions = await connection.run(
		"SELECT sql FROM duckdb_views() WHERE NOT internal AND sql IS NOT NULL UNION ALL SELECT macro_definition FROM duckdb_functions() WHERE NOT internal AND macro_definition IS NOT NULL",
	);
	const texts: string[] = [];
	for (const row of await definitions.getRows()) if (typeof row[0] === "string") texts.push(row[0]);
	return texts;
}

const LINE_SLACK_BYTES = 16 << 20;

function entriesScan(file: TranscriptRow): string {
	return `SELECT filename AS file, json FROM read_json([${literal(file.file)}], format = 'newline_delimited', records = false, columns = {'json': 'JSON'}, filename = true, maximum_object_size = ${file.bytes + LINE_SLACK_BYTES})`;
}

type ColumnType = "VARCHAR" | "BIGINT" | "DOUBLE" | "BOOLEAN" | "JSON";
type Cell = string | number | boolean | null;

interface Column {
	readonly name: string;
	readonly type: ColumnType;
}

const FILES: readonly Column[] = [
	{ name: "sid", type: "VARCHAR" },
	{ name: "agent", type: "VARCHAR" },
	{ name: "parent", type: "VARCHAR" },
	{ name: "file", type: "VARCHAR" },
	{ name: "bytes", type: "BIGINT" },
	{ name: "modified", type: "VARCHAR" },
];

const SESSIONS: readonly Column[] = [
	{ name: "sid", type: "VARCHAR" },
	{ name: "project", type: "VARCHAR" },
	{ name: "cwd", type: "VARCHAR" },
	{ name: "title", type: "VARCHAR" },
	{ name: "created", type: "VARCHAR" },
	{ name: "modified", type: "VARCHAR" },
	{ name: "status", type: "VARCHAR" },
	{ name: "assistant_turns", type: "BIGINT" },
	{ name: "size", type: "BIGINT" },
	{ name: "recap", type: "VARCHAR" },
	{ name: "parent", type: "VARCHAR" },
	{ name: "url", type: "VARCHAR" },
];

const TRANSCRIPTS: readonly Column[] = [
	{ name: "sid", type: "VARCHAR" },
	{ name: "agent", type: "VARCHAR" },
	{ name: "parent", type: "VARCHAR" },
	{ name: "title", type: "VARCHAR" },
	{ name: "created", type: "VARCHAR" },
	{ name: "parent_session", type: "VARCHAR" },
	{ name: "entries", type: "BIGINT" },
	{ name: "on_branch", type: "BIGINT" },
	{ name: "events", type: "BIGINT" },
	{ name: "url", type: "VARCHAR" },
];

const EVENTS: readonly Column[] = [
	{ name: "sid", type: "VARCHAR" },
	{ name: "agent", type: "VARCHAR" },
	{ name: "seq", type: "BIGINT" },
	{ name: "turn", type: "BIGINT" },
	{ name: "t", type: "VARCHAR" },
	{ name: "kind", type: "VARCHAR" },
	{ name: "entry", type: "VARCHAR" },
	{ name: "block", type: "BIGINT" },
	{ name: "line", type: "BIGINT" },
	{ name: "call_id", type: "VARCHAR" },
	{ name: "tool", type: "VARCHAR" },
	{ name: "status", type: "VARCHAR" },
	{ name: "result_entry", type: "VARCHAR" },
	{ name: "text", type: "VARCHAR" },
	{ name: "data", type: "JSON" },
	{ name: "url", type: "VARCHAR" },
];

function appendCell(appender: DuckDBAppender, type: ColumnType, value: Cell): void {
	if (value === null) {
		appender.appendNull();
		return;
	}
	switch (type) {
		case "VARCHAR":
		case "JSON":
			appender.appendVarchar(typeof value === "string" ? value : JSON.stringify(value));
			return;
		case "BIGINT":
			appender.appendBigInt(BigInt(value));
			return;
		case "DOUBLE":
			appender.appendDouble(Number(value));
			return;
		case "BOOLEAN":
			appender.appendBoolean(value === true);
			return;
		default: {
			const unreachable: never = type;
			throw new Error(`Unhandled column type ${String(unreachable)}`);
		}
	}
}

function appendRow(appender: DuckDBAppender, columns: readonly Column[], row: readonly Cell[]): void {
	for (const [index, column] of columns.entries()) appendCell(appender, column.type, row[index] ?? null);
	appender.endRow();
}

async function openTable(connection: DuckDBConnection, name: string, columns: readonly Column[]): Promise<DuckDBAppender> {
	await connection.run(
		`CREATE TABLE src.${identifier(name)} (${columns.map((column) => `${identifier(column.name)} ${column.type}`).join(", ")})`,
	);
	return connection.createAppender(name, "src");
}

async function createTable(
	connection: DuckDBConnection,
	name: string,
	columns: readonly Column[],
	rows: Iterable<readonly Cell[]>,
): Promise<void> {
	const appender = await openTable(connection, name, columns);
	try {
		for (const row of rows) appendRow(appender, columns, row);
	} finally {
		appender.closeSync();
	}
}

function str(record: EventRecord, key: string): string | null {
	const value = record[key];
	return typeof value === "string" ? value : null;
}

function int(record: EventRecord, key: string): number | null {
	const value = record[key];
	return typeof value === "number" ? value : null;
}

function dataJson(record: EventRecord, copied: string | null): string {
	if (copied === null) return JSON.stringify(record);
	return JSON.stringify(record, function (this: unknown, key, value) {
		return this === record && key === copied ? undefined : value;
	});
}

export function eventRow(transcript: TranscriptRow, seq: number, record: EventRecord): Cell[] {
	const text = str(record, "text");
	const source = str(record, "source");
	const copied = text !== null ? "text" : source !== null ? "source" : null;
	return [
		transcript.sid,
		transcript.agent,
		seq,
		int(record, "turn"),
		str(record, "t"),
		str(record, "kind"),
		str(record, "entry"),
		int(record, "block"),
		int(record, "line"),
		str(record, "callId"),
		str(record, "tool"),
		str(record, "status"),
		str(record, "resultEntry"),
		text ?? source,
		dataJson(record, copied),
		str(record, "url"),
	];
}

export interface SessionSelection {
	readonly sids: readonly string[];
	readonly agents: boolean;
}

async function locate(sids: readonly string[], cwd: string): Promise<SessionRef[]> {
	const refs: SessionRef[] = [];
	for (const prefix of sids) {
		const ref = await locateSession({ kind: "prefix", prefix }, cwd);
		if (!refs.some((known) => known.sid === ref.sid)) refs.push(ref);
	}
	return refs;
}

async function sessionRelations(
	connection: DuckDBConnection,
	selection: SessionSelection,
	needed: ReadonlySet<string>,
	cwd: string,
	signal: AbortSignal | undefined,
): Promise<void> {
	if (needed.size === 0) return;
	const refs = await locate(selection.sids, cwd);
	const wantsFiles = needed.has("files") || needed.has("entries");
	const wantsTree = wantsFiles || needed.has("transcripts") || needed.has("events");

	const filesAppender = needed.has("files") ? await openTable(connection, "files", FILES) : null;
	const sessionsAppender = needed.has("sessions") ? await openTable(connection, "sessions", SESSIONS) : null;
	const transcriptsAppender = needed.has("transcripts") ? await openTable(connection, "transcripts", TRANSCRIPTS) : null;
	const eventsAppender = needed.has("events") ? await openTable(connection, "events", EVENTS) : null;
	const files: TranscriptRow[] = [];
	try {
		for (const ref of refs) {
			const tree = wantsTree ? await transcriptTree(ref) : [];
			const info = sessionsAppender === null ? undefined : await readSessionInfo(ref.file);
			if (sessionsAppender !== null && info !== undefined)
				for (const row of metadata([info]))
					appendRow(sessionsAppender, SESSIONS, [
						row.sid,
						row.project,
						row.cwd,
						row.title,
						row.created,
						row.modified,
						row.status,
						row.assistantTurns,
						row.size,
						row.recap,
						row.parent,
						row.url,
					]);
			for (const transcript of selection.agents ? tree : tree.slice(0, 1)) {
				if (wantsFiles) {
					files.push(transcript);
					if (filesAppender !== null)
						appendRow(filesAppender, FILES, [
							transcript.sid,
							transcript.agent,
							transcript.parent,
							transcript.file,
							transcript.bytes,
							transcript.modified,
						]);
				}
				if (transcriptsAppender === null && eventsAppender === null) continue;
				const { loaded, records } = await transcriptEvents(transcript, { types: null, window: null }, signal);
				if (transcriptsAppender !== null) {
					const meta = loaded.meta;
					appendRow(transcriptsAppender, TRANSCRIPTS, [
						transcript.sid,
						transcript.agent,
						transcript.parent,
						meta.title,
						meta.created,
						meta.parentSession,
						meta.entries,
						meta.onBranch,
						records.length,
						transcriptUrl(transcript.sid, transcript.agent),
					]);
				}
				if (eventsAppender !== null)
					for (const [seq, record] of records.entries()) appendRow(eventsAppender, EVENTS, eventRow(transcript, seq, record));
			}
		}
	} finally {
		for (const appender of [filesAppender, sessionsAppender, transcriptsAppender, eventsAppender]) appender?.closeSync();
	}
	if (needed.has("entries")) {
		const scans = files.length === 0 ? ["SELECT NULL::VARCHAR AS file, NULL::JSON AS json WHERE false"] : files.map(entriesScan);
		await connection.run(
			`CREATE VIEW src.entries AS SELECT file, json, json->>'type' AS type, json->>'id' AS id, json->>'parentId' AS parent_id, json->>'timestamp' AS t FROM (${scans.join(" UNION ALL ")})`,
		);
	}
}

function valueType(value: unknown): ColumnType | null {
	if (value === null || value === undefined) return null;
	if (typeof value === "boolean") return "BOOLEAN";
	if (typeof value === "number") {
		if (Number.isSafeInteger(value)) return "BIGINT";
		return Number.isFinite(value) ? "DOUBLE" : "JSON";
	}
	return typeof value === "string" ? "VARCHAR" : "JSON";
}

function mergeType(type: ColumnType | null, value: unknown): ColumnType | null {
	const wanted = valueType(value);
	if (wanted === null || type === null) return type ?? wanted;
	if (type === wanted) return type;
	if ((type === "BIGINT" || type === "DOUBLE") && (wanted === "BIGINT" || wanted === "DOUBLE")) return "DOUBLE";
	return "JSON";
}

function cellOf(value: unknown, type: ColumnType): Cell {
	if (value === null || value === undefined) return null;
	if (type === "JSON") return JSON.stringify(value);
	return typeof value === "string" || typeof value === "number" || typeof value === "boolean" ? value : JSON.stringify(value);
}

async function callerTable(connection: DuckDBConnection, name: string, rows: readonly Readonly<Record<string, unknown>>[]): Promise<void> {
	const types = new Map<string, ColumnType | null>();
	for (const row of rows) for (const key of Object.keys(row)) types.set(key, mergeType(types.get(key) ?? null, row[key]));
	if (types.size === 0) throw new Error(`tables.${name}: the rows carry no columns`);
	const columns = [...types].map(([column, type]) => ({ name: column, type: type ?? "VARCHAR" }));
	const cells = function* (): Generator<Cell[]> {
		for (const row of rows) yield columns.map((column) => cellOf(row[column.name], column.type));
	};
	await createTable(connection, name, columns, cells());
}

export async function buildSources(
	connection: DuckDBConnection,
	selection: SessionSelection | null,
	tables: Readonly<Record<string, readonly Readonly<Record<string, unknown>>[]>>,
	sql: string,
	cwd: string,
	signal: AbortSignal | undefined,
): Promise<void> {
	await connection.run("DROP SCHEMA IF EXISTS src CASCADE");
	await connection.run("CREATE SCHEMA src");
	if (selection) {
		const referenced = await referencedRelations(connection, sql, await definitionTexts(connection));
		const needed = referenced === "all" ? new Set(SOURCE_RELATIONS) : referenced;
		await sessionRelations(connection, selection, needed, cwd, signal);
	}
	for (const [name, rows] of Object.entries(tables)) {
		if (SOURCE_RELATIONS.includes(name)) throw new Error(`tables.${name}: src.${name} is built from the named sessions; pick another name`);
		await callerTable(connection, name, rows);
	}
}

export async function dropSources(connection: DuckDBConnection): Promise<void> {
	await connection.run("DROP SCHEMA IF EXISTS src CASCADE");
}
