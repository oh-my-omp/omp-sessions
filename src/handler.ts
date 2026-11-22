// The session:// ProtocolHandler. The engine owns the URL grammar, the tree
// and every rendering (decision §1); each call here is one read process
// (decision §2) answering `resolve`, `readDirectory` or `locate` for the URL
// as omp parsed it. Failures read `session://: <kind>: <message>` (decision §15).
import { readFileSync } from "node:fs";
import type { InternalResource, InternalUrl, ProtocolHandler, ResolveContext, SchemeSpec } from "@oh-my-pi/pi-coding-agent/internal-urls";
import { type ShellFsDirEntry, ShellFsFileType } from "@oh-my-pi/pi-natives";
import { engineContext } from "./engine/context.ts";
import { failureText } from "./engine/failure.ts";
import type { Logger } from "./engine/frames.ts";
import type { Reply, UrlParts } from "./engine/protocol.ts";
import { type ReadOp, readRequest } from "./engine/read.ts";

/** The URL as omp's internal-URL parser hands it over; the engine parses its path. */
function parts(url: InternalUrl): UrlParts {
	return { href: url.href, host: url.rawHost || url.hostname, pathname: url.rawPathname ?? url.pathname, search: url.search };
}

function text(url: string, content: string): InternalResource {
	return { url, content, contentType: "text/plain", size: Buffer.byteLength(content) };
}

function jsonResource(url: string, value: unknown): InternalResource {
	const content = JSON.stringify(value, null, 2);
	return { url, content, contentType: "application/json", size: Buffer.byteLength(content) };
}

const RESERVED_HOSTS = new Set(["", "README", "sessions.jsonl", "find", "projects", "type"]);

function currentSessionId(context: ResolveContext | undefined): string {
	if (context?.sessionId) return context.sessionId;
	const fromSession = context?.session?.getSessionId?.();
	if (fromSession) return fromSession;
	if (context?.sessionFile) {
		const match = /_([0-9a-fA-F-]+)\.jsonl$/.exec(context.sessionFile);
		if (match?.[1]) return match[1];
	}
	throw new Error("session://: session://here/agents and session://here/events require an active sessionId");
}

function parseNonNegativeInt(raw: string, name: string, min: number, max?: number): number {
	if (!/^\d+$/.test(raw)) {
		throw new Error(`session://: ${name} must be an integer, got "${raw}"`);
	}
	const parsed = Number(raw);
	if (!Number.isSafeInteger(parsed) || parsed < min || (max !== undefined && parsed > max)) {
		const range = max !== undefined ? `${min}..${max}` : `>= ${min}`;
		throw new Error(`session://: ${name} must be in ${range}, got ${raw}`);
	}
	return parsed;
}

function parseEventsQuery(search: string): { agent?: string; type?: string; offset?: number; limit?: number } {
	if (!search) return {};
	const params = new URLSearchParams(search);
	const types: string[] = [];
	let agent: string | undefined;
	let offset: number | undefined;
	let limit: number | undefined;
	for (const [key, value] of params.entries()) {
		switch (key) {
			case "type":
				if (!value) throw new Error("session://: type= filter must not be empty");
				types.push(value);
				break;
			case "agent":
				if (agent !== undefined) throw new Error("session://: agent= may appear at most once");
				agent = value;
				break;
			case "offset":
				if (offset !== undefined) throw new Error("session://: offset= may appear at most once");
				offset = parseNonNegativeInt(value, "offset", 0);
				break;
			case "limit":
				if (limit !== undefined) throw new Error("session://: limit= may appear at most once");
				limit = parseNonNegativeInt(value, "limit", 1, 5000);
				break;
			default:
				throw new Error(`session://: unknown query parameter "${key}" on /events`);
		}
	}
	return {
		...(agent !== undefined && agent !== "" ? { agent } : {}),
		...(types.length > 0 ? { type: types.join(",") } : {}),
		...(offset !== undefined ? { offset } : {}),
		...(limit !== undefined ? { limit } : {}),
	};
}

export function routeSpecialRead(url: InternalUrl, context: ResolveContext | undefined): ReadOp | null {
	const host = url.rawHost || url.hostname;
	if (RESERVED_HOSTS.has(host)) return null;
	const pathname = url.rawPathname ?? url.pathname;
	const isHere = host === "here";
	const isAgents = pathname === "/agents" || (isHere && pathname === "/agents/");
	const isEvents = pathname === "/events" || (isHere && pathname === "/events/");
	if (!isAgents && !isEvents) return null;
	if (!isHere && isAgents && context?.skipDirectoryListing) return null;
	const sid = isHere ? currentSessionId(context) : host;
	if (isAgents) {
		if (url.search) throw new Error("session://: /agents does not accept query parameters");
		return { op: "agents", sid };
	}
	return { op: "events", sid, ...parseEventsQuery(url.search) };
}

function unexpected(op: string, reply: Reply): Error {
	return new Error(`session://: the engine answered ${op} with a ${reply.kind} reply`);
}

export class SessionUrlHandler implements ProtocolHandler {
	readonly scheme = "session";
	readonly spec: SchemeSpec = { backing: "file", selectors: "lines", immutable: true, linkable: false };
	readonly #logger: Logger;

	constructor(logger: Logger) {
		this.#logger = logger;
	}

	async #read(op: ReadOp, context: ResolveContext | undefined): Promise<Reply> {
		try {
			return await readRequest(op, engineContext(context?.cwd ?? process.cwd()), context?.signal, this.#logger);
		} catch (error) {
			throw new Error(`session://: ${failureText(error)}`);
		}
	}

	promptDoc(): string {
		return "`session://`: other OMP sessions on this machine (read-only). Read: `session://<id>/timeline` (`?type=thinking,irc`, `?since=1430&until=1500` or `?since=2h`, no raw `:` at URL end), `…/entry/<id>`, `…/events.jsonl?q=<jq>`, `…/agents` (`here/agents`), `…/events?type=&agent=&limit=&offset=` (`here/events`); grep `session://here/<date>/*/timeline` (filtered: `session://type/<types>/here/…`); find sessions with `archive`. Grammar: `read help://read/urls`.";
	}

	/** The whole URL grammar, served by omp at `help://read/urls`. */
	promptReference(): string {
		return readFileSync(new URL("../engine/url-grammar.md", import.meta.url), "utf8").trim();
	}

	async locate(url: InternalUrl, context?: ResolveContext): Promise<string | null> {
		try {
			const reply = await readRequest(
				{ op: "locate", url: parts(url) },
				engineContext(context?.cwd ?? process.cwd()),
				context?.signal,
				this.#logger,
			);
			return reply.kind === "located" ? reply.file : null;
		} catch {
			// Malformed, unknown or ambiguous: resolve reports the full reason.
			return null;
		}
	}

	async resolve(url: InternalUrl, context?: ResolveContext): Promise<InternalResource> {
		const special = routeSpecialRead(url, context);
		if (special) {
			const reply = await this.#read(special, context);
			switch (reply.kind) {
				case "rows":
					return jsonResource(url.href, { rows: reply.rows });
				case "page":
					return jsonResource(url.href, { rows: reply.rows, total: reply.total, offset: reply.offset, next: reply.next });
				default:
					throw unexpected(special.op, reply);
			}
		}
		const op: ReadOp = { op: "resolve", url: parts(url), skipDirectoryListing: context?.skipDirectoryListing === true };
		const reply = await this.#read(op, context);
		switch (reply.kind) {
			case "resource":
				return text(url.href, reply.content);
			case "directory":
				return { ...text(url.href, reply.content), isDirectory: true };
			default:
				throw unexpected("resolve", reply);
		}
	}

	async readDirectory(url: InternalUrl, context?: ResolveContext): Promise<ShellFsDirEntry[]> {
		const reply = await this.#read({ op: "readDirectory", url: parts(url) }, context);
		if (reply.kind !== "entries") throw unexpected("readDirectory", reply);
		return reply.entries.map((entry) => ({ name: entry.name, fileType: entry.dir ? ShellFsFileType.Dir : ShellFsFileType.File }));
	}
}
