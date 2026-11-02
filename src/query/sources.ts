// Schema `src`: the relations one session_query call reads, built at the start
// of the call and dropped at its end, so nothing in it outlives the call.
// For the sessions a call names:
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

async function createTable(
	connection: DuckDBConnection,
	name: string,
	columns: readonly Column[],
	rows: readonly (readonly Cell[])[],
): Promise<void> {
	await connection.run(
		`CREATE TABLE src.${identifier(name)} (${columns.map((column) => `${identifier(column.name)} ${column.type}`).join(", ")})`,
	);
	const appender = await connection.createAppender(name, "src");
	try {
		for (const row of rows) {
			for (const [index, column] of columns.entries()) appendCell(appender, column.type, row[index] ?? null);
			appender.endRow();
		}
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

function eventRow(transcript: TranscriptRow, seq: number, record: EventRecord): Cell[] {
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
		str(record, "text") ?? str(record, "source"),
		JSON.stringify(record),
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

async function sessionRelations(
	connection: DuckDBConnection,
	selection: SessionSelection,
	cwd: string,
	signal: AbortSignal | undefined,
): Promise<void> {
	const refs = await locate(selection.sids, cwd);
	const files: TranscriptRow[] = [];
	const sessions: Cell[][] = [];
	const transcripts: Cell[][] = [];
	const events: Cell[][] = [];
	for (const ref of refs) {
		const tree = await transcriptTree(ref);
		const info = await readSessionInfo(ref.file);
		for (const row of info ? metadata([info]) : [])
			sessions.push([
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
			files.push(transcript);
			const { loaded, records } = await transcriptEvents(transcript, { types: null, window: null }, signal);
			const meta = loaded.meta;
			transcripts.push([
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
			for (const [seq, record] of records.entries()) events.push(eventRow(transcript, seq, record));
		}
	}
	await createTable(
		connection,
		"files",
		FILES,
		files.map((file) => [file.sid, file.agent, file.parent, file.file, file.bytes, file.modified]),
	);
	await createTable(connection, "sessions", SESSIONS, sessions);
	await createTable(connection, "transcripts", TRANSCRIPTS, transcripts);
	await createTable(connection, "events", EVENTS, events);
	const scans = files.length === 0 ? ["SELECT NULL::VARCHAR AS file, NULL::JSON AS json WHERE false"] : files.map(entriesScan);
	await connection.run(
		`CREATE VIEW src.entries AS SELECT file, json, json->>'type' AS type, json->>'id' AS id, json->>'parentId' AS parent_id, json->>'timestamp' AS t FROM (${scans.join(" UNION ALL ")})`,
	);
}

/** Column type for caller rows: one JSON type when values disagree or are structured. */
function inferType(values: readonly unknown[]): ColumnType {
	const present = values.filter((value) => value !== null && value !== undefined);
	if (present.length === 0) return "VARCHAR";
	if (present.every((value) => typeof value === "boolean")) return "BOOLEAN";
	if (present.every((value) => typeof value === "number" && Number.isSafeInteger(value))) return "BIGINT";
	if (present.every((value) => typeof value === "number" && Number.isFinite(value))) return "DOUBLE";
	if (present.every((value) => typeof value === "string")) return "VARCHAR";
	return "JSON";
}

function cellOf(value: unknown, type: ColumnType): Cell {
	if (value === null || value === undefined) return null;
	if (type === "JSON") return JSON.stringify(value);
	return typeof value === "string" || typeof value === "number" || typeof value === "boolean" ? value : JSON.stringify(value);
}

async function callerTable(connection: DuckDBConnection, name: string, rows: readonly Readonly<Record<string, unknown>>[]): Promise<void> {
	const names = [...new Set(rows.flatMap((row) => Object.keys(row)))];
	if (names.length === 0) throw new Error(`tables.${name}: the rows carry no columns`);
	const columns = names.map((column) => ({ name: column, type: inferType(rows.map((row) => row[column])) }));
	await createTable(
		connection,
		name,
		columns,
		rows.map((row) => columns.map((column) => cellOf(row[column.name], column.type))),
	);
}

/** Builds schema `src` for one call. */
export async function buildSources(
	connection: DuckDBConnection,
	selection: SessionSelection | null,
	tables: Readonly<Record<string, readonly Readonly<Record<string, unknown>>[]>>,
	cwd: string,
	signal: AbortSignal | undefined,
): Promise<void> {
	await connection.run("DROP SCHEMA IF EXISTS src CASCADE");
	await connection.run("CREATE SCHEMA src");
	if (selection) await sessionRelations(connection, selection, cwd, signal);
	for (const [name, rows] of Object.entries(tables)) {
		if (SOURCE_RELATIONS.includes(name)) throw new Error(`tables.${name}: src.${name} is built from the named sessions; pick another name`);
		await callerTable(connection, name, rows);
	}
}

/** Drops schema `src` and everything the call put in it. */
export async function dropSources(connection: DuckDBConnection): Promise<void> {
	await connection.run("DROP SCHEMA IF EXISTS src CASCADE");
}
