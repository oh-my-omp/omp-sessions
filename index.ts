/**
 * omp-sessions: the omp sessions on this machine (~/.omp/agent/sessions), for reading and for SQL.
 *
 * `session://` (src/handler.ts): a read-only internal-URL scheme. `read` renders a session's
 * active-branch timeline (thinking, speech, IRC, dispatches and subagent results in full; tool
 * calls as one-line signatures), filterable by event type and time window, single entries in
 * full, events/metadata JSONL with jq, and finds sessions by prompt-history FTS, title and
 * recap; `grep`/`glob` walk one tree (projects/<bucket>/<date>/<id>/timeline) through
 * readDirectory. Registered at factory load in omp's process-global InternalUrlRouter, for main
 * and subagent sessions alike; never replaces an omp-owned scheme or another host's handler, and
 * a newer copy of this module replaces its own earlier registration.
 *
 * `session_query` (src/query/tool.ts): a top-level tool eval cells call as `tool.session_query`.
 * Registered synchronously at factory load with `defaultInactive`, and activated in
 * `session_start` through `applyActiveTools` (src/active-tools.ts), which orders names by a
 * canonical rule (builtins first in registry order, others sorted by name), so the base system
 * prompt stays the same across `/restart` and the prompt cache survives it.
 * `sessions`, `agents` and `events` return rows; `sql` runs DuckDB on the calling session's
 * in-memory database (src/query/database.ts) with the call's relations in schema `src`, rebuilt
 * from the session files on every call and dropped after it (src/query/sources.ts); tables the
 * caller creates in `main` last until `session_switch` or `session_shutdown` closes the
 * database. `export` writes one query result to a file the caller names.
 *
 * Files: reads session files, history.db (prompt FTS, recaps); writes a per-call temp JSONL for
 * jq (removed before the read returns) and the files `export` is asked to write. Never writes
 * below the sessions directory.
 *
 * Failure policy: hooks and tools never throw into omp; registration and activation errors are
 * logged with `pi.logger.warn`, query failures come back as tool errors.
 */
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { applyActiveTools } from "./src/active-tools.ts";
import { errorText } from "./src/errors.ts";
import { HANDLER_MARK, SessionUrlHandler } from "./src/handler.ts";
import { router } from "./src/omp-adapter.ts";
import { SessionDatabases } from "./src/query/database.ts";
import { sessionQueryTool, TOOL } from "./src/query/tool.ts";

function registerScheme(pi: ExtensionAPI): void {
	const routes = router();
	if (routes.isBuiltin("session")) {
		pi.logger.warn("omp-sessions: omp owns session://; not registering");
		return;
	}
	const existing = routes.getHandler("session");
	if (existing && !(HANDLER_MARK in existing)) {
		pi.logger.warn("omp-sessions: another host registered session://; not replacing it");
		return;
	}
	routes.register(new SessionUrlHandler());
	// Canonical order (oh-my-omp/omp-config#224 §U1): builtins keep host order
	// first; every other scheme sorts by name. Re-insert each non-builtin
	// handler in sorted order so the system-prompt list depends on the set,
	// not on binding timing.
	const ordered = [...routes.specs().keys()].filter((scheme) => !routes.isBuiltin(scheme)).sort();
	for (const scheme of ordered) {
		const handler = routes.getHandler(scheme);
		if (!handler) continue;
		routes.unregister(scheme);
		routes.register(handler);
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
	pi.on("session_switch", () => databases.close());
	pi.on("session_shutdown", () => databases.close());
}
