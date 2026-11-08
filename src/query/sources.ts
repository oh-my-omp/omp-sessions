// Schema `src`: the relations one session_query call reads, built at the start
// of the call and dropped at its end, so nothing in it outlives the call.
// For the sessions a call names, only the relations its SQL reaches exist:
//   src.files        each transcript file (root and subagents) with its sid and agent id
//   src.entries      a view reading those JSONL files in place at query time, one row per line
//   src.sessions     session metadata (the session:// listing rows)
//   src.transcripts  each transcript's tree position and branch counts
//   src.events       the active-branch events of every transcript (the events.jsonl records)
// The semantic relations come from this package's TypeScript loaders on each call; the
// view reads the authoritative files directly. Rows passed in `tables` become src.<name>.
import type { DuckDBAppender, DuckDBConnection } from "@duckdb/node-api";
import { readSessionInfo } from "../omp-adapter.ts";
import { type EventRecord, metadata, type TranscriptRow, transcriptEvents, transcriptTree, transcriptUrl } from "../relations.ts";
import { locateSession, type SessionRef } from "../routes.ts";
import { identifier, literal } from "./database.ts";

/** Names the semantic and raw relations take in schema `src`; caller tables cannot reuse them. */
export const SOURCE_RELATIONS: readonly string[] = ["files", "entries", "sessions", "transcripts", "events"];

/**
 * Words that let a stored view or macro reach `src` without naming one relation:
 * `src` itself, the catalog views and functions that list relations (`duckdb_*`,
 * `pg_*`, `sqlite_*`, `information_schema`, `SHOW`, `DESCRIBE`, `SUMMARIZE`), and
 * the table functions that take a relation name at run time (`query`,
 * `query_table`, `pragma_*`). A definition holding any of them builds every
 * relation: its `CREATE VIEW` text is not a SELECT `json_serialize_sql` accepts.
 */
const DEFINITION_REACHES_SRC =
	/\b(?:src|query|query_table|pragma_\w+|duckdb_\w+|pg_\w+|sqlite_\w+|information_schema|show|describe|summarize)\b/i;

/**
 * A relation the catalog owns: its rows list or describe the relations in `src`
 * (`information_schema.tables`, `pg_catalog.pg_class`, `sqlite_master`,
 * `duckdb_*` views), so a query that reads one builds every relation and its
 * rows match what a fully built `src` shows.
 */
const CATALOG_SCHEMA = /^(?:information_schema|pg_catalog)$/i;
const CATALOG_RELATION = /^(?:duckdb|pg|sqlite)_\w+$/i;

/**
 * The `src` relations one query's parse tree names, or `all` when that tree does
 * not name them: a statement DuckDB cannot serialize to a tree (anything but a
 * SELECT, so a CTAS included), a parse error, a reference resolved at run time
 * (a table function or a `SHOW`, whose relation names the tree lacks), or a
 * catalog relation (whose rows list the relations in `src`).
 *
 * The tree is DuckDB's own parse (`json_serialize_sql`), so quoted, commented,
 * spaced, catalog-qualified, CTE and subquery references arrive as the
 * `schema_name`/`table_name` they denote; the walk covers every node rather than
 * recognized shapes. A parse error is not reported from here: `runSql` reports
 * the query's own error.
 */
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
 * What one call's `src` must hold: `all` when the SQL's parse tree cannot name
 * its relations exactly, or when a stored view or macro definition can reach
 * `src`; else the relations the SQL names. Building a relation nobody reads only
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

/**
 * The SQL of every non-internal view and macro this database holds, in every
 * schema including `temp`: an object the caller kept across calls can reach
 * `src` without the call's own text naming it.
 */
async function definitionTexts(connection: DuckDBConnection): Promise<string[]> {
	const definitions = await connection.run(
		"SELECT sql FROM duckdb_views() WHERE NOT internal AND sql IS NOT NULL UNION ALL SELECT macro_definition FROM duckdb_functions() WHERE NOT internal AND macro_definition IS NOT NULL",
	);
	const texts: string[] = [];
	for (const row of await definitions.getRows()) if (typeof row[0] === "string") texts.push(row[0]);
	return texts;
}

/**
 * Slack above a file's size for the longest line DuckDB may meet in it: a
 * transcript still being written grows while the call runs.
 */
const LINE_SLACK_BYTES = 16 << 20;

/**
 * One file's scan for src.entries. DuckDB sizes every read buffer by
 * maximum_object_size, so a single corpus-wide bound (transcripts here hold
 * lines over 200 MB) runs a multi-file scan out of memory; each file is read
 * with a bound from its own size instead.
 */
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

/** One row of `columns` into an open appender. */
function appendRow(appender: DuckDBAppender, columns: readonly Column[], row: readonly Cell[]): void {
	for (const [index, column] of columns.entries()) appendCell(appender, column.type, row[index] ?? null);
	appender.endRow();
}

/** Creates `src.<name>` and opens its appender; the caller appends and then closes it. */
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

/**
 * `record` as JSON without the field `eventRow` copied into its text column:
 * `data` carries the rest, so no text lives in the table twice.
 */
function dataJson(record: EventRecord, copied: string | null): string {
	if (copied === null) return JSON.stringify(record);
	return JSON.stringify(record, function (this: unknown, key, value) {
		return this === record && key === copied ? undefined : value;
	});
}

export function eventRow(transcript: TranscriptRow, seq: number, record: EventRecord): Cell[] {
	const text = str(record, "text");
	const source = str(record, "source");
	// The text column shows `text`, else `source`; data drops exactly that field.
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

/** Which sessions a call reads: session ids or unique prefixes, with or without their subagents. */
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

/**
 * Builds the relations `needed` names for `selection`'s sessions. Tables and
 * their appenders open before the load loop and take each row as it is
 * produced, so events land one at a time instead of in one Cell[][] per call.
 * `src.sessions` costs readSessionInfo; `src.transcripts`/`src.events` cost one
 * transcriptEvents per transcript, serving both; nothing loads a session when
 * neither is named.
 */
async function sessionRelations(
	connection: DuckDBConnection,
	selection: SessionSelection,
	needed: ReadonlySet<string>,
	cwd: string,
	signal: AbortSignal | undefined,
): Promise<void> {
	if (needed.size === 0) return;
	const refs = await locate(selection.sids, cwd);
	// The entries view reads the file list it embeds, so it implies files.
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

/** The ColumnType one caller value wants: null and undefined vote for nothing. */
function valueType(value: unknown): ColumnType | null {
	if (value === null || value === undefined) return null;
	if (typeof value === "boolean") return "BOOLEAN";
	if (typeof value === "number") {
		if (Number.isSafeInteger(value)) return "BIGINT";
		return Number.isFinite(value) ? "DOUBLE" : "JSON";
	}
	return typeof value === "string" ? "VARCHAR" : "JSON";
}

/** Column type for a caller column so far: the type a whole-column pass would pick. */
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

/** Builds schema `src` for one call: only the relations `sql` (and persisted main objects) reference. */
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

/** Drops schema `src` and everything the call put in it. */
export async function dropSources(connection: DuckDBConnection): Promise<void> {
	await connection.run("DROP SCHEMA IF EXISTS src CASCADE");
}
