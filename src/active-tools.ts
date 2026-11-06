import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

/**
 * Activates `names` in a canonical order that depends only on the tool set, not on the order in
 * which tools were registered. omp builds the host's `tools` array in the order of the last
 * `setActiveTools` argument, so registration timing (late vs eager extension binding) would
 * otherwise change the base system prompt across restarts.
 *
 * Order: builtin tools from `getAllTools()` in their registry order first; then every other
 * requested name (extension, SDK, MCP, or unknown to `getAllTools()`) sorted by code point,
 * deduplicated.
 */
export async function applyActiveTools(pi: Pick<ExtensionAPI, "getAllTools" | "setActiveTools">, names: Iterable<string>): Promise<void> {
	const requested = new Set(names);
	const placed = new Set<string>();
	const builtins: string[] = [];
	const builtinNames = new Set<string>();
	for (const tool of pi.getAllTools()) {
		if (tool.sourceInfo.source !== "builtin") continue;
		builtinNames.add(tool.name);
		if (requested.has(tool.name) && !placed.has(tool.name)) {
			placed.add(tool.name);
			builtins.push(tool.name);
		}
	}
	const others = [...requested].filter((name) => !builtinNames.has(name)).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
	await pi.setActiveTools([...builtins, ...others]);
}
