import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import { SessionDatabases } from "../src/query/database.ts";

describe("session_query engine", () => {
	test("bounded memory, private spill directory removed on close", async () => {
		const databases = new SessionDatabases();
		const facts = await databases.run("engine-test", undefined, async (connection) => {
			const settings = await connection.runAndReadAll(
				"SELECT current_setting('memory_limit') AS memory, current_setting('temp_directory') AS spill",
			);
			const [memory, spill] = settings.getRowsJson()[0] as [string, string];
			const locked = await connection.run("SET memory_limit = '64GB'").then(
				() => "changed",
				(error: unknown) => String(error),
			);
			return { memory, spill, spillExists: fs.existsSync(spill), locked };
		});
		expect(bytes(facts.memory)).toBeGreaterThan(3.96e9);
		expect(bytes(facts.memory)).toBeLessThan(4.04e9);
		expect(facts.spill).toContain("omp-sessions-duckdb-");
		expect(facts.spillExists).toBe(true);
		expect(facts.locked).toContain("configuration has been locked");
		await databases.close();
		expect(fs.existsSync(facts.spill)).toBe(false);
	});
});

function bytes(setting: string): number {
	const units: Readonly<Record<string, number>> = { KiB: 1024, MiB: 1024 ** 2, GiB: 1024 ** 3, TiB: 1024 ** 4 };
	const [amount, unit] = setting.split(" ") as [string, string];
	return Number(amount) * (units[unit] ?? 0);
}
