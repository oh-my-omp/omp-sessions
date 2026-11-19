import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { applyActiveTools } from "./omomp-shared/active-tools.ts";
import { registerInternalScheme } from "./omomp-shared/internal-url.ts";
import { errorText } from "./src/errors.ts";
import { SessionUrlHandler } from "./src/handler.ts";
import { router } from "./src/omp-adapter.ts";
import { SessionDatabases } from "./src/query/database.ts";
import { sessionQueryTool, TOOL } from "./src/query/tool.ts";

function registerScheme(pi: ExtensionAPI): void {
	const result = registerInternalScheme(router(), "omp-sessions", new SessionUrlHandler());
	switch (result.kind) {
		case "registered":
			return;
		case "builtin":
		case "held":
			pi.logger.warn(`omp-sessions: ${result.reason}`);
			return;
		default: {
			const unreachable: never = result;
			throw new Error(`Unhandled register result ${String(unreachable)}`);
		}
	}
}

export default function ompSessions(pi: ExtensionAPI): void {
	try {
		registerScheme(pi);
	} catch (error) {
		pi.logger.warn("omp-sessions: session:// registration failed", { error: errorText(error) });
	}
	const databases = new SessionDatabases();
	pi.registerTool({ ...sessionQueryTool(databases), defaultInactive: true });
	pi.on("session_start", async () => {
		try {
			const active = new Set(pi.getActiveTools());
			const names = pi
				.getAllTools()
				.map((tool) => tool.name)
				.filter((name) => active.has(name) || name === TOOL);
			await applyActiveTools(pi, names);
		} catch (error) {
			pi.logger.warn("omp-sessions: session_query activation failed", { error: errorText(error) });
		}
	});
	pi.on("session_switch", () => void databases.close());
	pi.on("session_shutdown", () => void databases.close());
}
