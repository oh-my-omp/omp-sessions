// The DuckDB database of one persisted omp session: in memory, opened by the
// first session_query call of that session, closed when the session ends
// (session_switch, session_shutdown) or when a call arrives under another
// session id. Its configuration bounds the engine's memory at ENGINE_MEMORY,
// spilling beyond it into a private temporary directory that lives as long as
// the database, and lets SQL statements read only below the sessions directory
// and write no file of their own (the engine's own spill files stay inside that
// directory): memory_limit and temp_directory, then allowed_directories,
// then enable_external_access off, then lock_configuration. Tables the caller
// creates in schema `main` are that session's intermediate results; schema
// `src` belongs to the call that builds it (sources.ts). Calls on one database
// run one at a time, in arrival order.
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { type DuckDBConnection, DuckDBInstance } from "@duckdb/node-api";
import { getSessionsDir } from "../omp-adapter.ts";

/** A SQL string literal. */
export function literal(value: string): string {
	return `'${value.replaceAll("'", "''")}'`;
}

/** A SQL identifier. */
export function identifier(value: string): string {
	return `"${value.replaceAll('"', '""')}"`;
}

/** The most memory one session's engine holds; tables, joins and sorts past it spill to disk. */
const ENGINE_MEMORY = "4GB";

interface Engine {
	readonly instance: DuckDBInstance;
	readonly connection: DuckDBConnection;
	/** The engine's spill directory, removed when it closes. */
	readonly spill: string;
}

interface Open {
	readonly sessionId: string;
	readonly engine: Promise<Engine>;
	/** Settles when the last queued call has finished. */
	tail: Promise<void>;
}

async function openEngine(): Promise<Engine> {
	let spill: string | null = null;
	let instance: DuckDBInstance | null = null;
	let connection: DuckDBConnection | null = null;
	try {
		spill = fs.mkdtempSync(path.join(os.tmpdir(), "omp-sessions-duckdb-"));
		instance = await DuckDBInstance.create(":memory:");
		connection = await instance.connect();
		await connection.run(`SET memory_limit = ${literal(ENGINE_MEMORY)}`);
		await connection.run(`SET temp_directory = ${literal(spill)}`);
		const sessions = `${getSessionsDir().replace(/\/+$/, "")}/`;
		await connection.run(`SET allowed_directories = [${literal(sessions)}]`);
		await connection.run("SET autoinstall_known_extensions = false");
		await connection.run("SET autoload_known_extensions = false");
		await connection.run("SET enable_external_access = false");
		await connection.run("SET lock_configuration = true");
		return { instance, connection, spill };
	} catch (error) {
		connection?.closeSync();
		instance?.closeSync();
		if (spill !== null) fs.rmSync(spill, { recursive: true, force: true });
		throw error;
	}
}

/** Settles once the engine has closed and its spill directory is gone. */
function closeEngine(open: Open): Promise<void> {
	return open.engine.then(
		(engine) => {
			engine.connection.interrupt();
			return open.tail.finally(() => {
				engine.connection.closeSync();
				engine.instance.closeSync();
				fs.rmSync(engine.spill, { recursive: true, force: true });
			});
		},
		// The engine never opened: nothing to close; the call that awaited it reported the error.
		() => undefined,
	);
}

/** The databases of the sessions this extension instance serves; at most one is open. */
export class SessionDatabases {
	#open: Open | null = null;

	/** Runs `work` on the database of session `sessionId`, after every earlier call on it. */
	async run<T>(sessionId: string, signal: AbortSignal | undefined, work: (connection: DuckDBConnection) => Promise<T>): Promise<T> {
		if (this.#open?.sessionId !== sessionId) {
			void this.close();
			this.#open = { sessionId, engine: openEngine(), tail: Promise.resolve() };
		}
		const open = this.#open;
		const previous = open.tail;
		const { promise, resolve: release } = Promise.withResolvers<void>();
		open.tail = promise;
		try {
			await previous;
			const { connection } = await open.engine;
			signal?.throwIfAborted();
			const interrupt = (): void => connection.interrupt();
			signal?.addEventListener("abort", interrupt, { once: true });
			try {
				return await work(connection);
			} finally {
				signal?.removeEventListener("abort", interrupt);
			}
		} finally {
			release();
		}
	}

	/** Ends the open database: interrupts its running call and closes it once its queue drains; settles when it has closed. */
	close(): Promise<void> {
		const open = this.#open;
		if (!open) return Promise.resolve();
		this.#open = null;
		return closeEngine(open);
	}
}
