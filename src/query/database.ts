import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { type DuckDBConnection, DuckDBInstance } from "@duckdb/node-api";
import { getSessionsDir } from "../omp-adapter.ts";

export function literal(value: string): string {
	return `'${value.replaceAll("'", "''")}'`;
}

export function identifier(value: string): string {
	return `"${value.replaceAll('"', '""')}"`;
}

const ENGINE_MEMORY = "4GB";

interface Engine {
	readonly instance: DuckDBInstance;
	readonly connection: DuckDBConnection;
	readonly spill: string;
}

interface Open {
	readonly sessionId: string;
	readonly engine: Promise<Engine>;
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
		() => undefined,
	);
}

export class SessionDatabases {
	#open: Open | null = null;

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

	close(): Promise<void> {
		const open = this.#open;
		if (!open) return Promise.resolve();
		this.#open = null;
		return closeEngine(open);
	}
}
