/**
 * omp-sessions: the omp sessions on this machine (~/.omp/agent/sessions), for reading and for SQL.
 *
 * The Rust engine in engine/ is the only reader of the session files (decision §1); this shell
 * registers the two entry points with omp, turns their calls into engine requests, runs the
 * engine processes and puts their errors on omp's result surfaces.
 *
 * `session://` (src/handler.ts): a read-only internal-URL scheme. `read` renders a session's
 * active-branch timeline, filterable by event type and time window, single entries in full,
 * events JSONL with jq, `.../agents` transcript tree JSON, and `.../events` paginated events JSON
 * (with `session://here/agents` and `session://here/events` for the active session); `grep`/`glob`
 * walk one tree (projects/<bucket>/<date>/<id>/timeline) through readDirectory. The URL grammar is
 * served at `help://read/urls`. Each call runs one engine read process. Registered at factory load
 * in omp's process-global InternalUrlRouter, for main and subagent sessions alike; never replaces an
 * omp-owned scheme or another owner's handler, and a newer copy of this module replaces its own
 * earlier registration.
 *
 * `session_query` (src/query/tool.ts): a top-level DuckDB SQL and export tool eval cells call as
 * `tool.session_query`. Registered synchronously at factory load with `defaultInactive`, and
 * activated in `session_start` through `applyActiveTools` (omomp-shared/active-tools.ts), which
 * orders names by a canonical rule (builtins first in registry order, others sorted by name), so
 * the base system prompt stays the same across `/restart` and the prompt cache survives it. SQL
 * and `export_to` run in the session's query process (src/engine/query.ts), which keeps the tables
 * the caller creates in `main` until `session_switch` or `session_shutdown` ends it; its guide is
 * registered at `tool://session_query/sql`.
 *
 * Files: the shell opens no session file. The engine binary is fetched into engine/bin on first
 * use when the package carries none (src/engine/binary.ts); the engine writes its cold cache
 * under omp's agent cache root and the files `export_to` is asked to write, never below the
 * sessions directory.
 *
 * Failure policy: hooks and tools never throw into omp; registration and activation errors are
 * logged with `pi.logger.warn`, call failures come back as `session://: <kind>: <message>` read
 * errors and `session_query <sql|export> failed: <kind>: <message>` tool errors.
 */
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { InternalUrlRouter } from "@oh-my-pi/pi-coding-agent/internal-urls";
import { applyActiveTools } from "./omomp-shared/active-tools.ts";
import { registerInternalScheme, registerToolTopics } from "./omomp-shared/internal-url.ts";
import { QueryProcesses } from "./src/engine/query.ts";
import { errorText } from "./src/errors.ts";
import { SessionUrlHandler } from "./src/handler.ts";
import { sessionQuerySqlTopic, sessionQueryTool, TOOL } from "./src/query/tool.ts";

function registerScheme(pi: ExtensionAPI): void {
	const result = registerInternalScheme(InternalUrlRouter.instance(), "omp-sessions", new SessionUrlHandler(pi.logger));
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
	try {
		registerToolTopics("omp-sessions", [sessionQuerySqlTopic()]);
	} catch (error) {
		pi.logger.warn("omp-sessions: tool:// topic registration failed", { error: errorText(error) });
	}
	const processes = new QueryProcesses(pi.logger);
	pi.registerTool({ ...sessionQueryTool(processes, pi.logger), defaultInactive: true });
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
	pi.on("session_switch", () => processes.close());
	pi.on("session_shutdown", () => processes.close());
}
