// The session_query tool: this machine's omp sessions as relations for eval
// cells, which call it as `tool.session_query({...})` and read `.details`.
// The request is parsed once here into a discriminated union and becomes one
// engine request: `agents` and `events` run in a read process, `sql` and
// `export` in the calling omp session's query process (decision §2, §3).
// Failures read `session_query <action> failed: <kind>: <message>` (decision §15).
import { readFileSync } from "node:fs";
import { z } from "@oh-my-pi/omptype/zod";
import type { ExtensionContext, ToolDefinition } from "@oh-my-pi/pi-coding-agent";
import { tr } from "../../omomp-shared/i18n.ts";
import { engineContext } from "../engine/context.ts";
import { EngineFailure, failureText } from "../engine/failure.ts";
import type { Logger } from "../engine/frames.ts";
import type { Reply } from "../engine/protocol.ts";
import type { QueryOp, QueryProcesses } from "../engine/query.ts";
import { type ReadOp, readRequest } from "../engine/read.ts";

export const TOOL = "session_query";

const MAX_SESSIONS = 50;

/** The operations this tool serves; `action` selects the request shape. `sessions` only answers where to go instead. */
const ACTIONS = ["sessions", "agents", "events", "sql", "export"] as const;

const SESSIONS_GONE =
	"the sessions action is gone: list sessions, search prompts and read recaps with omp's `archive` eval global (archive.sessions, archive.search, archive.prompts, archive.recaps); its session ids work as sid here";

const sid = z.string().min(1).describe("session id or unique id prefix");
const scalar = z.union([z.string(), z.number(), z.boolean(), z.null()]);

const query = z
	.string()
	.min(1)
	.describe(
		"`sql` and `export`: the SQL to run; several statements may be separated by `;` and the last one's result comes back; `export` runs a single SELECT",
	);
const path = z.string().min(1).describe("`export`: absolute file path outside the omp sessions directory");
const format = z.enum(["parquet", "csv", "json"]);

const from = z
	.object({
		sids: z.array(sid).min(1).max(MAX_SESSIONS).describe("session ids or unique id prefixes; at most 50"),
		agents: z.boolean().optional().describe("also read every subagent transcript; default true"),
	})
	.strict()
	.optional()
	.describe(
		"`sql` and `export`: the sessions whose relations become schema `src`, read again on every call; omitted: no session relations",
	);

/** The rows `tables` JSON text carries, once decoded. */
const tablesValue = z
	.record(z.string().regex(/^[A-Za-z_][A-Za-z0-9_]{0,63}$/), z.array(z.record(z.string(), z.unknown())).min(1))
	.optional();
/** The bindings `params` JSON text carries, once decoded. */
const paramsValue = z.union([z.array(scalar), z.record(z.string(), scalar)]).optional();

/** What the model sees: one object, `action` selecting which other fields apply. */
const parameters = z.object({
	action: z
		.enum(ACTIONS)
		.describe(
			"the operation: `agents` a transcript tree, `events` a page of events, `sql` runs a query, `export` writes a query result to a file; `sessions` is gone, use omp's `archive` eval global",
		),
	sid: sid.optional().describe("`agents` and `events`: session id or unique id prefix"),
	agent: z.string().optional().describe("`events`: subagent id; omitted: the root transcript"),
	type: z.string().optional().describe("`events`: session:// type filter, e.g. `thinking,irc` or `call:bash`"),
	offset: z
		.number()
		.int()
		.min(0)
		.optional()
		.describe("`events`: 0-based index of the first record; default 0; the previous page's `next` continues from there"),
	limit: z
		.number()
		.int()
		.min(1)
		.optional()
		.describe("row cap: `events` default 500 (at most 5000), `sql` default 1000 (at most 100000); `export` takes none"),
	query: query.optional(),
	from,
	tables: z
		.string()
		.optional()
		.describe(
			'`sql` and `export`: JSON text of rows from the cell, e.g. `{"t": [{"a": 1}, {"a": 2}]}` becomes table `src.t`; booleans, integers, numbers and strings keep their type, a column of anything else becomes JSON',
		),
	params: z
		.string()
		.optional()
		.describe(
			'`sql` and `export`: JSON text of the bindings for one statement, e.g. `[1, "a"]` for positional `?` or `{"v": "a"}` for named `$v`; values are strings, numbers, booleans or null',
		),
	path: path.optional(),
	format: format.optional().describe("`export`: the file to write: parquet, csv or json"),
});

const agentsRequest = z.object({ action: z.literal("agents"), sid }).strict();
const eventsRequest = z
	.object({
		action: z.literal("events"),
		sid,
		agent: z.string().optional(),
		type: z.string().optional(),
		offset: z.number().int().min(0).optional(),
		limit: z.number().int().min(1).max(5000).optional(),
	})
	.strict();
const sqlRequest = z
	.object({
		action: z.literal("sql"),
		query,
		from,
		tables: tablesValue,
		params: paramsValue,
		limit: z.number().int().min(1).max(100_000).optional(),
	})
	.strict();
const exportRequest = z
	.object({ action: z.literal("export"), query, path, format, from, tables: tablesValue, params: paramsValue })
	.strict();

/** One request shape per served `action`, what `parseRequest` hands the rest of this file. */
const requests = {
	agents: agentsRequest,
	events: eventsRequest,
	sql: sqlRequest,
	export: exportRequest,
} as const;
type Request = z.infer<typeof agentsRequest> | z.infer<typeof eventsRequest> | z.infer<typeof sqlRequest> | z.infer<typeof exportRequest>;
type Details = Record<string, unknown>;

/** Field-first text for a zod failure, e.g. `query: must be removed`; the path names the field. */
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

/** The value a `params` or `tables` argument carries; text that is not JSON fails naming the field. */
function jsonText(value: string | undefined, name: string): unknown {
	if (value === undefined) return undefined;
	try {
		return JSON.parse(value);
	} catch (error) {
		throw badRequest(`${name}: not JSON text (${error instanceof Error ? error.message : String(error)})`);
	}
}

/** The action a raw argument object names, for the failure text of a call that does not parse. */
function actionOf(raw: unknown): string {
	const parsed = z.object({ action: z.string() }).safeParse(raw);
	return parsed.success ? parsed.data.action : "call";
}

/**
 * The one entry both `approval` and `execute` use: parse the flat `parameters`
 * object, decode its JSON text, then admit the result as the request shape its
 * `action` names. Fields that belong to another action and missing required
 * fields fail here as `bad_request`, naming the field.
 */
function parseRequest(raw: unknown): Request {
	const flat = parameters.safeParse(raw);
	if (!flat.success) throw badRequest(issues(flat.error));
	if (flat.data.action === "sessions") throw badRequest(SESSIONS_GONE);
	// The flat parse keeps every declared key, absent ones as `undefined`; a
	// branch admits only the fields it declares, so drop those and decode first.
	const decoded = Object.fromEntries(
		Object.entries({
			...flat.data,
			params: jsonText(flat.data.params, "params"),
			tables: jsonText(flat.data.tables, "tables"),
		}).filter(([, value]) => value !== undefined),
	);
	const request = requests[flat.data.action].safeParse(decoded);
	if (!request.success) throw badRequest(issues(request.error));
	return request.data;
}

const DESCRIPTION = [
	"SQL and row queries over this machine's omp sessions, called from eval: `(await tool.session_query({action, …})).details` in a JS or Python cell.",
	"`sql` runs DuckDB on the session files in place (relations in schema `src`, rebuilt each call); tables you create in `main` last for this session; `export` writes a result to a file you name.",
].join("\n");

const TOPICS = [
	{ name: "actions", when: "before a session_query call" },
	{ name: "sql", when: "before sql or export: the src relations, what persists, what SQL may do" },
] as const;

const TOPIC_POINTERS = [
	"Topics — read the one a call needs before making it:",
	...TOPICS.map((topic) => `- \`xd://${TOOL}/${topic.name}\` — ${topic.when}`),
].join("\n");

function topics(): Readonly<Record<string, string>> {
	return Object.fromEntries(
		TOPICS.map((topic) => [topic.name, readFileSync(new URL(`./topics/${topic.name}.md`, import.meta.url), "utf8")]),
	);
}

function unexpected(action: string, reply: Reply): Error {
	return new Error(`the engine answered ${action} with a ${reply.kind} reply`);
}

/** The engine op of a request. */
function readOp(request: Extract<Request, { action: "agents" | "events" }>): ReadOp {
	switch (request.action) {
		case "agents":
			return { op: "agents", sid: request.sid };
		case "events":
			return {
				op: "events",
				sid: request.sid,
				agent: request.agent,
				type: request.type,
				offset: request.offset,
				limit: request.limit,
			};
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

function queryOp(request: Extract<Request, { action: "sql" | "export" }>): QueryOp {
	switch (request.action) {
		case "sql":
			return { op: "sql", query: request.query, from: request.from, tables: request.tables, params: request.params, limit: request.limit };
		case "export":
			return {
				op: "export",
				query: request.query,
				path: request.path,
				format: request.format,
				from: request.from,
				tables: request.tables,
				params: request.params,
			};
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

/** The `details` of a reply, in the shapes the `actions` topic documents. */
function details(action: Request["action"], reply: Reply): Details {
	switch (reply.kind) {
		case "rows":
			return { rows: reply.rows };
		case "page":
			return { rows: reply.rows, total: reply.total, offset: reply.offset, next: reply.next };
		case "sqlRows":
			return { kind: "rows", columns: reply.columns, rows: reply.rows, truncated: reply.truncated };
		case "changes":
			return { kind: "changes", changes: reply.changes };
		case "exported":
			return { path: reply.path, format: reply.format, rows: reply.rows, columns: reply.columns };
		case "resource":
		case "directory":
		case "entries":
		case "located":
			throw unexpected(action, reply);
		default: {
			const unreachable: never = reply;
			throw new Error(`Unhandled reply ${String(unreachable)}`);
		}
	}
}

async function run(
	request: Request,
	processes: QueryProcesses,
	logger: Logger,
	ctx: ExtensionContext,
	signal: AbortSignal | undefined,
): Promise<Details> {
	switch (request.action) {
		case "agents":
		case "events":
			return details(request.action, await readRequest(readOp(request), engineContext(ctx.cwd), signal, logger));
		case "sql":
		case "export": {
			const sessionId = ctx.sessionManager.getSessionId();
			const reply = await processes.call(sessionId, queryOp(request), engineContext(ctx.cwd, sessionId), signal);
			return details(request.action, reply);
		}
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
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
function summary(request: Request, result: Details): string {
	return (
		jsonWithin(result, SUMMARY_CHARS) ??
		`${request.action}: more than ${SUMMARY_CHARS} chars of JSON in details; call from eval and read .details`
	);
}

/** Writing a file needs write approval; every other action only reads session files. */
function approval(args: unknown): "read" | "write" {
	let request: Request;
	try {
		request = parseRequest(args);
	} catch {
		return actionOf(args) === "sessions" ? "read" : "write";
	}
	switch (request.action) {
		case "agents":
		case "events":
		case "sql":
			return "read";
		case "export":
			return "write";
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

/** Query failures are expected (bad SQL, unknown session) and come back as tool errors, not log warnings. */
export function sessionQueryTool(
	processes: QueryProcesses,
	logger: Logger,
): ToolDefinition<typeof parameters> & { docTopics(): Readonly<Record<string, string>> } {
	return {
		name: TOOL,
		label: tr("Session Query", "会话查询"),
		description: `${DESCRIPTION}\n\n${TOPIC_POINTERS}`,
		approval,
		parameters,
		docTopics: topics,
		async execute(_toolCallId, rawParams, signal, _onUpdate, ctx) {
			let request: Request;
			try {
				request = parseRequest(rawParams);
			} catch (error) {
				return { content: [{ type: "text", text: `session_query ${actionOf(rawParams)} failed: ${failureText(error)}` }], isError: true };
			}
			try {
				const result = await run(request, processes, logger, ctx, signal);
				return { content: [{ type: "text", text: summary(request, result) }], details: result };
			} catch (error) {
				return { content: [{ type: "text", text: `session_query ${request.action} failed: ${failureText(error)}` }], isError: true };
			}
		},
	};
}
