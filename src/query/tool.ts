// The session_query tool: DuckDB SQL and export over this machine's omp
// sessions, called directly or from eval (`(await tool.session_query({...})).details`).
// Runs in the calling omp session's query process (decision §2, §3); when
// `tables` specifies named event tables, those rows are fetched via `readRequest`.
// Failures read `session_query <sql|export> failed: <kind>: <message>` (decision §15).
import { closeSync, type Dirent, existsSync, openSync, readdirSync, readFileSync, readSync } from "node:fs";
import * as path from "node:path";
import { z } from "@oh-my-pi/omptype/zod";
import type { ExtensionContext, ToolDefinition } from "@oh-my-pi/pi-coding-agent";
import { tr } from "../../omomp-shared/i18n.ts";
import type { ToolTopicDoc } from "../../omomp-shared/internal-url.ts";
import { engineContext } from "../engine/context.ts";
import { EngineFailure, failureText } from "../engine/failure.ts";
import type { Logger } from "../engine/frames.ts";
import type { Context, Reply, Selection } from "../engine/protocol.ts";
import type { QueryOp, QueryProcesses } from "../engine/query.ts";
import { readRequest } from "../engine/read.ts";

export const TOOL = "session_query";

const BUILTIN_SRC_TABLES = new Set(["files", "entries", "sessions", "transcripts", "events"]);
const TABLE_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

const scalar = z.union([z.string(), z.number(), z.boolean(), z.null()]);
const sid = z.string().min(1).describe("session id or unique id prefix");
const tableSpec = z
	.object({
		name: z
			.string()
			.regex(TABLE_NAME_RE, "table name must start with a letter or _ followed by up to 63 letters, digits, or _")
			.describe("table name created under schema src"),
		sids: z.array(sid).min(1).optional().describe("session ids or prefixes to materialize; defaults to the call's scope"),
		agent: z.string().optional().describe("subagent id; omitted loads root transcript"),
	})
	.strict();

export const parameters = z.object({
	query: z.string().min(1).describe("DuckDB SQL to run (last statement result returned; single SELECT for export_to)"),
	from: z.enum(["here", "all"]).optional().describe("session scope when sids is omitted: 'here' (current project) or 'all'"),
	sids: z.array(sid).min(1).optional().describe("explicit session ids or unique id prefixes to load into schema src"),
	agents: z.array(z.string()).optional().describe("subagent ids to include ([] loads root transcripts only)"),
	tables: z.array(tableSpec).optional().describe("named event tables materialized into schema src"),
	params: z.record(z.string(), scalar).optional().describe("named $key SQL bindings for a single statement"),
	export_to: z
		.object({
			path: z.string().min(1).describe("absolute file path outside the omp sessions directory"),
			format: z.enum(["parquet", "csv", "json"]).describe("output file format"),
		})
		.strict()
		.optional()
		.describe("write query result to a file instead of returning rows"),
});

const strictParameters = parameters.strict();
export type SessionQueryRequest = z.infer<typeof strictParameters>;
type Details = Record<string, unknown>;

export const DESCRIPTION =
	"Query OMP session history with DuckDB SQL (schema src, persistent main) or export to parquet/csv/json. Read tool://session_query/sql before querying.";

export function sessionQuerySqlTopic(): ToolTopicDoc {
	return {
		tool: TOOL,
		topic: "sql",
		title: "SQL over the session files",
		when: "before querying or exporting with session_query: src relations, persistent main schema, and allowed SQL",
		body: () => readFileSync(new URL("./topics/sql.md", import.meta.url), "utf8").trim(),
	};
}

/** Field-first text for a zod failure, e.g. `query: must be string`; the path names the field. */
function issues(error: { issues: readonly { path: readonly PropertyKey[]; message: string }[] }): string {
	return error.issues
		.map((issue) => {
			const field = issue.path.map(String).join(".");
			return field === "" ? issue.message : `${field}: ${issue.message}`;
		})
		.join("; ");
}

function badRequest(message: string): EngineFailure {
	return new EngineFailure("bad_request", message);
}

export function parseRequest(raw: unknown): SessionQueryRequest {
	const parsed = strictParameters.safeParse(raw);
	if (!parsed.success) throw badRequest(issues(parsed.error));
	return parsed.data;
}

function readHeaderSessionId(filePath: string): string | null {
	let fd: number | undefined;
	try {
		fd = openSync(filePath, "r");
		const buffer = Buffer.alloc(4096);
		const bytes = readSync(fd, buffer, 0, buffer.length, 0);
		if (bytes <= 0) return null;
		const text = buffer.toString("utf8", 0, bytes);
		const newline = text.indexOf("\n");
		const firstLine = (newline === -1 ? text : text.slice(0, newline)).trim();
		if (!firstLine) return null;
		const parsed = JSON.parse(firstLine) as { id?: unknown };
		return typeof parsed.id === "string" && parsed.id.length > 0 ? parsed.id : null;
	} catch {
		return null;
	} finally {
		if (fd !== undefined) closeSync(fd);
	}
}

function extractSidFromFilename(name: string, fullPath: string): string | null {
	const stamped = /_([0-9a-fA-F-]+)\.jsonl$/.exec(name);
	if (stamped?.[1]) return stamped[1];
	return readHeaderSessionId(fullPath);
}

function scanBucketSids(dir: string): string[] {
	if (!existsSync(dir)) return [];
	const seen = new Set<string>();
	const sids: string[] = [];
	let entries: Dirent[];
	try {
		entries = readdirSync(dir, { withFileTypes: true });
	} catch {
		return [];
	}
	const files = entries
		.filter((entry) => entry.isFile() && entry.name.endsWith(".jsonl"))
		.map((entry) => entry.name)
		.sort()
		.reverse();
	for (const name of files) {
		const sidValue = extractSidFromFilename(name, path.join(dir, name));
		if (sidValue && !seen.has(sidValue)) {
			seen.add(sidValue);
			sids.push(sidValue);
		}
	}
	return sids;
}

function discoverScopeSids(scope: "here" | "all", context: Context): string[] {
	if (scope === "here") {
		return scanBucketSids(context.hereDir);
	}
	if (!existsSync(context.sessionsDir)) return [];
	const seen = new Set<string>();
	const sids: string[] = [];
	let buckets: Dirent[];
	try {
		buckets = readdirSync(context.sessionsDir, { withFileTypes: true });
	} catch {
		return [];
	}
	for (const bucket of buckets) {
		if (!bucket.isDirectory()) continue;
		for (const sidValue of scanBucketSids(path.join(context.sessionsDir, bucket.name))) {
			if (!seen.has(sidValue)) {
				seen.add(sidValue);
				sids.push(sidValue);
			}
		}
	}
	return sids;
}

function resolveSelection(request: SessionQueryRequest, context: Context): Selection | undefined {
	if (!request.sids && !request.from) return undefined;
	const sids = request.sids ? request.sids : discoverScopeSids(request.from ?? "here", context);
	const includeAgents = request.agents === undefined ? true : request.agents.some((agent) => agent !== "");
	return { sids, agents: includeAgents };
}

async function materializeTables(
	specs: NonNullable<SessionQueryRequest["tables"]>,
	fallbackSids: readonly string[] | undefined,
	context: Context,
	signal: AbortSignal | undefined,
	logger: Logger,
): Promise<Record<string, Array<Record<string, unknown>>>> {
	const out: Record<string, Array<Record<string, unknown>>> = {};
	for (const spec of specs) {
		if (BUILTIN_SRC_TABLES.has(spec.name)) {
			throw badRequest(`tables.${spec.name}: src.${spec.name} is built from the named sessions; pick another name`);
		}
		if (Object.hasOwn(out, spec.name)) {
			throw badRequest(`tables.${spec.name}: duplicate table name`);
		}
		const targetSids = spec.sids ?? fallbackSids;
		if (!targetSids || targetSids.length === 0) {
			throw badRequest(`tables.${spec.name}: provide sids on the table or a session scope`);
		}
		const rows: Array<Record<string, unknown>> = [];
		for (const targetSid of targetSids) {
			let offset = 0;
			for (;;) {
				const reply = await readRequest(
					{
						op: "events",
						sid: targetSid,
						...(spec.agent !== undefined && spec.agent !== "" ? { agent: spec.agent } : {}),
						offset,
						limit: 5000,
					},
					context,
					signal,
					logger,
				);
				if (reply.kind !== "page") {
					throw new Error(`the engine answered events with a ${reply.kind} reply`);
				}
				rows.push(...reply.rows);
				if (reply.next === null || reply.next <= offset) break;
				offset = reply.next;
			}
		}
		if (rows.length === 0) {
			throw badRequest(`tables.${spec.name}: no matching events found`);
		}
		out[spec.name] = rows;
	}
	return out;
}

function details(opName: string, reply: Reply): Details {
	switch (reply.kind) {
		case "sqlRows":
			return { kind: "rows", columns: reply.columns, rows: reply.rows, truncated: reply.truncated };
		case "changes":
			return { kind: "changes", changes: reply.changes };
		case "exported":
			return { path: reply.path, format: reply.format, rows: reply.rows, columns: reply.columns };
		case "rows":
		case "page":
		case "resource":
		case "directory":
		case "entries":
		case "located":
			throw new Error(`the engine answered ${opName} with a ${reply.kind} reply`);
		default: {
			const unreachable: never = reply;
			throw new Error(`Unhandled reply ${String(unreachable)}`);
		}
	}
}

async function run(
	request: SessionQueryRequest,
	processes: QueryProcesses,
	logger: Logger,
	ctx: ExtensionContext,
	signal: AbortSignal | undefined,
): Promise<Details> {
	const sessionId = ctx.sessionManager.getSessionId();
	const context = engineContext(ctx.cwd, sessionId);
	const from = resolveSelection(request, context);
	const fallbackSids = from?.sids.length ? from.sids : sessionId ? [sessionId] : undefined;
	const tables = request.tables ? await materializeTables(request.tables, fallbackSids, context, signal, logger) : undefined;
	const op: QueryOp = request.export_to
		? {
				op: "export",
				query: request.query,
				path: request.export_to.path,
				format: request.export_to.format,
				...(from ? { from } : {}),
				...(tables ? { tables } : {}),
				...(request.params ? { params: request.params } : {}),
			}
		: {
				op: "sql",
				query: request.query,
				...(from ? { from } : {}),
				...(tables ? { tables } : {}),
				...(request.params ? { params: request.params } : {}),
			};
	const reply = await processes.call(sessionId, op, context, signal);
	return details(op.op, reply);
}

/** The most JSON text a direct call shows. */
const SUMMARY_CHARS = 8192;

/**
 * `JSON.stringify(value)` when it is at most `budget` chars, else null.
 * Serialization stops once the text passes the budget, so a large result is
 * never serialized whole; a string longer than what is left is not escaped.
 * The value is JSON data — strings, numbers, booleans, null, arrays, plain
 * objects, Dates — which is what `Details` holds; boxed primitives and a
 * `toJSON` returning undefined would not match `JSON.stringify` here.
 */
export function jsonWithin(value: unknown, budget: number): string | null {
	const parts: string[] = [];
	let length = 0;
	const push = (text: string): boolean => {
		length += text.length;
		parts.push(text);
		return length <= budget;
	};
	const write = (item: unknown): boolean => {
		if (typeof item === "string") return length + item.length + 2 <= budget && push(JSON.stringify(item));
		if (item === null || typeof item !== "object" || ("toJSON" in item && typeof item.toJSON === "function"))
			return push(JSON.stringify(item) ?? "null");
		if (Array.isArray(item)) {
			if (!push("[")) return false;
			for (let at = 0; at < item.length; at++) if ((at > 0 && !push(",")) || !write(item[at])) return false;
			return push("]");
		}
		if (!push("{")) return false;
		let first = true;
		for (const [key, field] of Object.entries(item)) {
			if (field === undefined || typeof field === "function" || typeof field === "symbol") continue;
			if (!push(`${first ? "" : ","}${JSON.stringify(key)}:`) || !write(field)) return false;
			first = false;
		}
		return push("}");
	};
	return write(value) ? parts.join("") : null;
}

/** The text a direct (non-eval) call shows; eval cells read `details`. */
function summary(isExport: boolean, result: Details): string {
	return (
		jsonWithin(result, SUMMARY_CHARS) ??
		`${isExport ? "export" : "sql"}: more than ${SUMMARY_CHARS} chars of JSON in details; call from eval and read .details`
	);
}

/** Writing an export file needs write approval; SQL queries only read session files. */
export function approval(args: unknown): "read" | "write" {
	try {
		const request = parseRequest(args);
		return request.export_to !== undefined ? "write" : "read";
	} catch {
		if (args !== null && typeof args === "object" && "export_to" in args && (args as { export_to?: unknown }).export_to !== undefined) {
			return "write";
		}
		return "read";
	}
}

/** Query failures are expected (bad SQL, unknown session) and come back as tool errors, not log warnings. */
export function sessionQueryTool(processes: QueryProcesses, logger: Logger): ToolDefinition<typeof parameters> {
	return {
		name: TOOL,
		label: tr("Session Query", "会话查询"),
		description: DESCRIPTION,
		approval,
		parameters,
		async execute(_toolCallId, rawParams, signal, _onUpdate, ctx) {
			let request: SessionQueryRequest;
			try {
				request = parseRequest(rawParams);
			} catch (error) {
				const opName =
					rawParams !== null &&
					typeof rawParams === "object" &&
					"export_to" in rawParams &&
					(rawParams as { export_to?: unknown }).export_to !== undefined
						? "export"
						: "sql";
				return { content: [{ type: "text", text: `session_query ${opName} failed: ${failureText(error)}` }], isError: true };
			}
			const opName = request.export_to ? "export" : "sql";
			try {
				const result = await run(request, processes, logger, ctx, signal);
				return { content: [{ type: "text", text: summary(Boolean(request.export_to), result) }], details: result };
			} catch (error) {
				return { content: [{ type: "text", text: `session_query ${opName} failed: ${failureText(error)}` }], isError: true };
			}
		},
	};
}
