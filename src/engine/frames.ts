// Frames on the engine's stdio (decision §4): one JSON object per line. The
// types come from engine/src/protocol.rs through the generated protocol.ts.
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { EngineFailure } from "./failure.ts";
import { type Context, type ExportFormat, type Op, PROTOCOL, type Reply, type Request, type Response } from "./protocol.ts";

export type Logger = ExtensionAPI["logger"];

export function requestFrame(id: number, context: Context, op: Op): string {
	const request: Request = { id, protocol: PROTOCOL, context, ...op };
	return `${JSON.stringify(request)}\n`;
}

function isObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** One response frame, parsed field by field into the generated types. */
export function parseResponse(line: string): Response {
	let value: unknown;
	try {
		value = JSON.parse(line);
	} catch {
		throw new EngineFailure("engine_exited", `the engine wrote a line that is not a response frame: ${line.slice(0, 200)}`);
	}
	if (isObject(value) && typeof value.id === "number") {
		if (value.ok === true && isObject(value.result) && typeof value.result.kind === "string") {
			return { id: value.id, ok: true, result: reply(value.result) };
		}
		if (value.ok === false && isObject(value.error) && typeof value.error.kind === "string" && typeof value.error.message === "string") {
			return { id: value.id, ok: false, error: { kind: errorKind(value.error.kind), message: value.error.message } };
		}
	}
	throw new EngineFailure("engine_exited", `the engine wrote a line that is not a response frame: ${line.slice(0, 200)}`);
}

const ERROR_KINDS = ["bad_request", "not_found", "ambiguous", "refused", "query_failed", "io", "protocol_mismatch", "aborted"] as const;

function errorKind(kind: string): (typeof ERROR_KINDS)[number] {
	const known = ERROR_KINDS.find((candidate) => candidate === kind);
	if (known === undefined) throw new EngineFailure("engine_exited", `the engine answered with an unknown error kind ${kind}`);
	return known;
}

function malformed(field: string): EngineFailure {
	return new EngineFailure("engine_exited", `the engine wrote a reply whose ${field} does not match the protocol`);
}

function str(value: unknown, field: string): string {
	if (typeof value !== "string") throw malformed(field);
	return value;
}

function num(value: unknown, field: string): number {
	if (typeof value !== "number") throw malformed(field);
	return value;
}

function strings(value: unknown, field: string): string[] {
	if (!Array.isArray(value)) throw malformed(field);
	return value.map((item) => str(item, field));
}

function objects(value: unknown, field: string): Record<string, unknown>[] {
	if (!Array.isArray(value)) throw malformed(field);
	return value.map((item) => {
		if (!isObject(item)) throw malformed(field);
		return item;
	});
}

function exportFormat(value: unknown): ExportFormat {
	if (value === "parquet" || value === "csv" || value === "json") return value;
	throw malformed("format");
}

function reply(result: Record<string, unknown>): Reply {
	const kind = result.kind;
	switch (kind) {
		case "resource":
		case "directory":
			return { kind, content: str(result.content, "content") };
		case "entries":
			return {
				kind,
				entries: objects(result.entries, "entries").map((entry) => {
					if (typeof entry.dir !== "boolean") throw malformed("entries");
					return { name: str(entry.name, "entries"), dir: entry.dir };
				}),
			};
		case "located":
			return { kind, file: result.file === null ? null : str(result.file, "file") };
		case "rows":
			return { kind, rows: objects(result.rows, "rows") };
		case "page":
			return {
				kind,
				rows: objects(result.rows, "rows"),
				total: num(result.total, "total"),
				offset: num(result.offset, "offset"),
				next: result.next === null ? null : num(result.next, "next"),
			};
		case "sqlRows":
			if (typeof result.truncated !== "boolean") throw malformed("truncated");
			return { kind, columns: strings(result.columns, "columns"), rows: objects(result.rows, "rows"), truncated: result.truncated };
		case "changes":
			return { kind, changes: num(result.changes, "changes") };
		case "exported":
			return {
				kind,
				path: str(result.path, "path"),
				format: exportFormat(result.format),
				rows: num(result.rows, "rows"),
				columns: strings(result.columns, "columns"),
			};
		default:
			throw malformed(`kind ${String(kind)}`);
	}
}

/** The reply of a response, or its error as a failure. */
export function replyOf(response: Response): Reply {
	if (response.ok) return response.result;
	throw new EngineFailure(response.error.kind, response.error.message);
}

/**
 * Forwards engine stderr lines to omp's log (decision §13): each request's JSON
 * log line as info with its fields, anything else (a panic) as a warning.
 */
export function forwardLog(logger: Logger, text: string): void {
	for (const line of text.split("\n")) {
		if (line.trim() === "") continue;
		let fields: unknown;
		try {
			fields = JSON.parse(line);
		} catch {
			fields = undefined;
		}
		if (isObject(fields)) logger.info("omp-sessions engine", fields);
		else logger.warn("omp-sessions engine", { stderr: line });
	}
}
