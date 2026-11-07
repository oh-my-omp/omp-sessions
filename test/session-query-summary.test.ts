// session_query summary: the budgeted serializer returns exactly
// JSON.stringify's text when it fits the budget and null as soon as it does not.
import { describe, expect, test } from "bun:test";
import { jsonWithin } from "../src/query/tool.ts";

const values: unknown[] = [
	null,
	0,
	-1.5,
	Number.NaN,
	true,
	"",
	'quote " backslash \\ newline \n tab \t nul \u0000 emoji 😀 lone \ud800',
	[],
	{},
	[1, "two", null, undefined, () => 1, Symbol("s"), [[]], {}],
	{ a: 1, skip: undefined, fn: () => 1, sym: Symbol("s"), nested: { deep: [{ x: "y" }] }, 'key "q"': "v" },
	{ at: new Date("2026-11-08T00:00:00.000Z"), when: [new Date(0)] },
	{
		kind: "rows",
		columns: ["sid", "text"],
		rows: [
			["019a", "x".repeat(5000)],
			["019b", "é".repeat(3000)],
		],
		truncated: false,
	},
	{ rows: Array.from({ length: 300 }, (_, at) => ({ seq: at, text: `line ${at}\n` })), total: 300, offset: 0, next: null },
];

describe("session_query summary", () => {
	test("text equals JSON.stringify when it fits, null when it does not", () => {
		for (const value of values) {
			const full = JSON.stringify(value);
			for (const budget of [0, 1, 2, full.length - 1, full.length, full.length + 1, 8192]) {
				if (budget < 0) continue;
				expect({ full, budget, text: jsonWithin(value, budget) }).toEqual({ full, budget, text: full.length <= budget ? full : null });
			}
		}
	});

	test("a string far past the budget is refused without being serialized whole", () => {
		const huge = "a".repeat(50_000_000);
		expect(jsonWithin({ rows: [[huge]] }, 8192)).toBeNull();
		expect(jsonWithin(huge, 8192)).toBeNull();
	});
});
