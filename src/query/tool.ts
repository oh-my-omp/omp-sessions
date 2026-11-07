// The session_query tool: this machine's omp sessions as relations for eval
// cells, which call it as `tool.session_query({...})` and read `.details`.
// `sessions`, `agents` and `events` return rows from the TypeScript loaders;
// `sql` and `export` run DuckDB on the calling session's database
// (database.ts) with this call's relations in schema `src` (sources.ts). The
// request is parsed once here into a discriminated union.
import { readFileSync } from "node:fs";
import { type ExtensionContext, type ToolDefinition, z } from "@oh-my-pi/pi-coding-agent";
import { errorText } from "../errors.ts";
import { parseTypeFilter } from "../filter.ts";
import { tr } from "../i18n.ts";
import { ALL_SESSIONS, sessionRows, transcriptEvents, transcriptRow, transcriptTree } from "../relations.ts";
import { agentFile, locateSession } from "../routes.ts";
import type { SessionDatabases } from "./database.ts";
import { type Bindings, exportResult, runSql } from "./run.ts";
import { buildSources, dropSources, type SessionSelection } from "./sources.ts";

export const TOOL = "session_query";

const MAX_SESSIONS = 50;

const sid = z.string().min(1).describe("session id or unique id prefix");
const scalar = z.union([z.string(), z.number(), z.boolean(), z.null()]);
const from = z
	.object({
		sids: z.array(sid).min(1).max(MAX_SESSIONS),
		agents: z.boolean().optional().describe("also read every subagent transcript; default true"),
	})
	.strict()
	.optional()
	.describe("sessions whose relations this call builds in schema src");
const tables = z
	.record(z.string().regex(/^[A-Za-z_][A-Za-z0-9_]{0,63}$/), z.array(z.record(z.string(), z.unknown())).min(1))
	.optional()
	.describe("rows from the cell, each becoming table src.<name>");
const params = z.union([z.array(scalar), z.record(z.string(), scalar)]).optional();

const schema = z.union([
	z
		.object({
			action: z.literal("sessions"),
			project: z.string().min(1).optional().describe("`.` this project, a bucket name or a path; omitted: every project"),
			text: z.string().min(1).optional().describe("words matched against typed prompts, title and recap"),
			since: z.string().optional().describe("ISO date/time; modified at or after"),
			until: z.string().optional().describe("ISO date/time; modified before"),
			limit: z.number().int().min(1).max(ALL_SESSIONS).optional().describe("default 200"),
		})
		.strict(),
	z.object({ action: z.literal("agents"), sid }).strict(),
	z
		.object({
			action: z.literal("events"),
			sid,
			agent: z.string().optional().describe("subagent id; omitted: the root transcript"),
			type: z.string().optional().describe("session:// type filter, e.g. `thinking,irc` or `call:bash`"),
			offset: z.number().int().min(0).optional(),
			limit: z.number().int().min(1).max(5000).optional().describe("default 500"),
		})
		.strict(),
	z
		.object({
			action: z.literal("sql"),
			query: z.string().min(1),
			from,
			tables,
			params,
			limit: z.number().int().min(1).max(100_000).optional().describe("row cap; default 1000"),
		})
		.strict(),
	z
		.object({
			action: z.literal("export"),
			query: z.string().min(1).describe("one SELECT"),
			path: z.string().min(1).describe("absolute file path outside the omp sessions directory"),
			format: z.enum(["parquet", "csv", "json"]),
			from,
			tables,
			params,
		})
		.strict(),
]);

type Request = z.infer<typeof schema>;
type Details = Record<string, unknown>;

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

function parseInstant(value: string | undefined, name: string): number | null {
	if (value === undefined) return null;
	const at = Date.parse(value);
	if (Number.isNaN(at)) throw new Error(`${name}: ${JSON.stringify(value)} is not an ISO date or time`);
	return at;
}

function selection(value: { sids: string[]; agents?: boolean } | undefined): SessionSelection | null {
	return value === undefined ? null : { sids: value.sids, agents: value.agents ?? true };
}

async function rowAction(
	request: Extract<Request, { action: "sessions" | "agents" | "events" }>,
	ctx: ExtensionContext,
	signal: AbortSignal | undefined,
): Promise<Details> {
	switch (request.action) {
		case "sessions":
			return {
				rows: await sessionRows(
					{
						project: request.project ?? null,
						text: request.text ?? null,
						since: parseInstant(request.since, "since"),
						until: parseInstant(request.until, "until"),
						limit: request.limit ?? 200,
					},
					ctx.cwd,
				),
			};
		case "agents":
			return { rows: await transcriptTree(await locateSession({ kind: "prefix", prefix: request.sid }, ctx.cwd)) };
		case "events": {
			const ref = await locateSession({ kind: "prefix", prefix: request.sid }, ctx.cwd);
			const agent = request.agent ?? "";
			const file = agent === "" ? ref.file : await agentFile(ref, agent);
			const types = request.type === undefined ? null : parseTypeFilter([request.type]);
			const { records } = await transcriptEvents(await transcriptRow(ref.sid, agent, null, file), { types, window: null }, signal);
			const offset = request.offset ?? 0;
			const end = offset + (request.limit ?? 500);
			return { rows: records.slice(offset, end), total: records.length, offset, next: end < records.length ? end : null };
		}
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

async function run(
	request: Request,
	databases: SessionDatabases,
	ctx: ExtensionContext,
	signal: AbortSignal | undefined,
): Promise<Details> {
	switch (request.action) {
		case "sessions":
		case "agents":
		case "events":
			return rowAction(request, ctx, signal);
		case "sql":
		case "export":
			return databases.run(ctx.sessionManager.getSessionId(), signal, async (connection) => {
				try {
					await buildSources(connection, selection(request.from), request.tables ?? {}, request.query, ctx.cwd, signal);
					const bindings: Bindings | null = request.params ?? null;
					return request.action === "sql"
						? { ...(await runSql(connection, request.query, bindings, request.limit ?? 1000)) }
						: { ...(await exportResult(connection, request.query, bindings, request.path, request.format)) };
				} finally {
					await dropSources(connection);
				}
			});
		default: {
			const unreachable: never = request;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

/** The text a direct (non-eval) call shows; eval cells read `details`. */
function summary(request: Request, details: Details): string {
	const shown = JSON.stringify(details);
	return shown.length <= 8192 ? shown : `${request.action}: ${shown.length} chars of JSON in details; call from eval and read .details`;
}

/** Writing a file needs write approval; every other action only reads session files. */
function approval(args: unknown): "read" | "write" {
	const parsed = schema.safeParse(args);
	if (!parsed.success) return "write";
	switch (parsed.data.action) {
		case "sessions":
		case "agents":
		case "events":
		case "sql":
			return "read";
		case "export":
			return "write";
		default: {
			const unreachable: never = parsed.data;
			throw new Error(`Unhandled action ${String(unreachable)}`);
		}
	}
}

/** Query failures are expected (bad SQL, unknown session) and come back as tool errors, not log warnings. */
export function sessionQueryTool(
	databases: SessionDatabases,
): ToolDefinition<typeof schema> & { docTopics(): Readonly<Record<string, string>> } {
	return {
		name: TOOL,
		label: tr("Session Query", "会话查询"),
		description: `${DESCRIPTION}\n\n${TOPIC_POINTERS}`,
		approval,
		parameters: schema,
		docTopics: topics,
		async execute(_toolCallId, rawParams, signal, _onUpdate, ctx) {
			const request = schema.safeParse(rawParams);
			if (!request.success) return { content: [{ type: "text", text: `session_query: ${request.error.message}` }], isError: true };
			try {
				const details = await run(request.data, databases, ctx, signal);
				return { content: [{ type: "text", text: summary(request.data, details) }], details };
			} catch (error) {
				return { content: [{ type: "text", text: `session_query ${request.data.action} failed: ${errorText(error)}` }], isError: true };
			}
		},
	};
}
