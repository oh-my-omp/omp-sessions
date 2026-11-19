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
		return "`session://`: other OMP sessions on this machine (read-only). Read: `session://<id>/timeline` (thinking, speech, IRC, calls as signatures; `?type=thinking,irc` filters by kind, `?since=1430&until=1500` or `?since=2h` by time, no raw `:` at the URL's end), `…/entry/<id>` for one call's full args/result, `…/events.jsonl?q=<jq>`; grep `session://here/<date>/*/timeline` (filtered: `session://type/<types>/here/…`); find sessions with the `archive` eval global. Grammar: `read xd://read/urls`.";
	}

	/** The whole URL grammar, served by omp at `xd://read/urls`. */
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
