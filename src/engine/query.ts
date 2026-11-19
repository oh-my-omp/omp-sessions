// Query processes (decision §3): one per omp session, started by its first
// `sql` or `export` call, holding the DuckDB database whose `main` keeps the
// caller's tables, views and macros between calls. It ends at session_switch,
// session_shutdown, or when a call arrives under another session id: the shell
// cancels what is still running, closes stdin, and terminates the process tree
// if it has not exited within 5 s. Calls run in arrival order in the engine;
// an aborted call sends a `cancel` frame and the engine answers it `aborted`.
// A process that exits unexpectedly fails its in-flight calls, and the next
// call, with `engine_exited`; the call after that starts a new process.
import { ptree } from "@oh-my-pi/pi-utils";
import { engineBinary } from "./binary.ts";
import { aborted, EngineFailure } from "./failure.ts";
import { forwardLog, type Logger, parseResponse, replyOf, requestFrame } from "./frames.ts";
import type { Cancel, Context, Op, Reply, Response } from "./protocol.ts";
import { exitDescription } from "./read.ts";

export type QueryOp = Extract<Op, { op: "sql" | "export" }>;

/** How long a closing process may take to finish before its tree is terminated. */
const CLOSE_GRACE_MS = 5000;

type Child = ptree.ChildProcess<"pipe">;

interface Pending {
	readonly resolve: (response: Response) => void;
	readonly reject: (failure: EngineFailure) => void;
}

/** Lines of a byte stream, each passed to `line` as it completes. */
async function eachLine(stream: ReadableStream<Uint8Array>, line: (text: string) => void): Promise<void> {
	const decoder = new TextDecoder();
	let buffered = "";
	for await (const chunk of stream) {
		buffered += decoder.decode(chunk, { stream: true });
		let end = buffered.indexOf("\n");
		while (end >= 0) {
			line(buffered.slice(0, end));
			buffered = buffered.slice(end + 1);
			end = buffered.indexOf("\n");
		}
	}
	buffered += decoder.decode();
	if (buffered !== "") line(buffered);
}

class QueryProcess {
	readonly #child: Child;
	readonly #pending = new Map<number, Pending>();
	#nextId = 1;
	/** Set once the process has exited: why every later call fails. */
	#exited: EngineFailure | null = null;
	readonly #closed: Promise<void>;

	constructor(child: Child, logger: Logger) {
		this.#child = child;
		const stderr = child.stderr;
		const logs = stderr === undefined ? Promise.resolve() : eachLine(stderr, (line) => forwardLog(logger, line));
		const responses = eachLine(child.stdout, (line) => this.#answer(line));
		this.#closed = Promise.allSettled([child.exited, logs, responses]).then(() => this.#exit());
	}

	#answer(line: string): void {
		if (line.trim() === "") return;
		let response: Response;
		try {
			response = parseResponse(line);
		} catch (error) {
			if (error instanceof EngineFailure) this.#failAll(error);
			return;
		}
		const pending = this.#pending.get(response.id);
		if (pending === undefined) return;
		this.#pending.delete(response.id);
		pending.resolve(response);
	}

	#failAll(failure: EngineFailure): void {
		for (const pending of this.#pending.values()) pending.reject(failure);
		this.#pending.clear();
	}

	#exit(): void {
		this.#exited = new EngineFailure(
			"engine_exited",
			`the query process exited (${exitDescription(this.#child.exitCode, this.#child.proc.signalCode)}); the tables, views and macros in main are lost`,
		);
		this.#failAll(this.#exited);
	}

	#write(frame: string): boolean {
		try {
			this.#child.stdin.write(frame);
			this.#child.stdin.flush();
			return true;
		} catch {
			return false;
		}
	}

	#cancel(id: number): void {
		const cancel: Cancel = { op: "cancel", id };
		this.#write(`${JSON.stringify(cancel)}\n`);
	}

	async call(op: QueryOp, context: Context, signal: AbortSignal | undefined): Promise<Reply> {
		if (this.#exited !== null) throw this.#exited;
		if (signal?.aborted) throw aborted(`the ${op.op} call was cancelled before it started`);
		const id = this.#nextId++;
		const { promise, resolve, reject } = Promise.withResolvers<Response>();
		this.#pending.set(id, { resolve, reject });
		const onAbort = (): void => this.#cancel(id);
		signal?.addEventListener("abort", onAbort, { once: true });
		try {
			if (!this.#write(requestFrame(id, context, op))) {
				this.#pending.delete(id);
				await this.#closed;
				throw this.#exited ?? new EngineFailure("engine_exited", "the query process stopped reading requests");
			}
			return replyOf(await promise);
		} finally {
			signal?.removeEventListener("abort", onAbort);
		}
	}

	/** Cancels what is in flight, closes stdin, and terminates the tree after the grace period; settles once it has exited. */
	async close(): Promise<void> {
		for (const id of this.#pending.keys()) this.#cancel(id);
		try {
			this.#child.stdin.end();
		} catch {
			// Already gone: the exit handling below still settles.
		}
		const timer = setTimeout(() => this.#child.kill(undefined, 0), CLOSE_GRACE_MS);
		try {
			await this.#closed;
		} finally {
			clearTimeout(timer);
		}
	}

	get exited(): boolean {
		return this.#exited !== null;
	}
}

async function start(logger: Logger): Promise<QueryProcess> {
	const binary = await engineBinary(undefined);
	return new QueryProcess(ptree.spawn([binary, "query"], { stdin: "pipe", stderr: "full" }), logger);
}

interface Open {
	readonly sessionId: string;
	readonly process: Promise<QueryProcess>;
}

/** The query processes of the sessions this extension instance serves; at most one is running. */
export class QueryProcesses {
	readonly #logger: Logger;
	#open: Open | null = null;

	constructor(logger: Logger) {
		this.#logger = logger;
	}

	async call(sessionId: string, op: QueryOp, context: Context, signal: AbortSignal | undefined): Promise<Reply> {
		if (this.#open?.sessionId !== sessionId) {
			void this.close();
			this.#open = { sessionId, process: start(this.#logger) };
		}
		const open = this.#open;
		let process: QueryProcess;
		try {
			process = await open.process;
		} catch (error) {
			if (this.#open === open) this.#open = null;
			throw error;
		}
		try {
			return await process.call(op, context, signal);
		} finally {
			// A process that has exited answers no later call: the next one starts a new process.
			if (process.exited && this.#open === open) this.#open = null;
		}
	}

	/** Ends the running process, if any; settles once it has exited. */
	close(): Promise<void> {
		const open = this.#open;
		if (open === null) return Promise.resolve();
		this.#open = null;
		return open.process.then(
			(process) => process.close(),
			// It never started: the call that awaited it reported the failure.
			() => undefined,
		);
	}
}
