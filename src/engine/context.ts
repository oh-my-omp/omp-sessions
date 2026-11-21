// The omp facts every engine request carries (decision §9): omp owns its
// directory layout, so the shell reads it from omp's exports and the engine
// never derives it.
import * as path from "node:path";
import { sessionDirForCwd } from "@oh-my-pi/pi-coding-agent/session/session-paths";
import { getSessionsDir, getTinyModelsCacheDir } from "@oh-my-pi/pi-utils";
import type { Context } from "./protocol.ts";

/**
 * The engine's cold cache directory: `omp-sessions` under omp's agent cache
 * root. omp exports no cache root of its own, only directories inside it, so
 * the root is the parent of one of them; it follows omp's default and XDG layouts.
 */
function cacheDir(): string {
	return path.join(path.dirname(getTinyModelsCacheDir()), "omp-sessions");
}

/** The request context of a call from `cwd`; query-process requests also name the omp session. */
export function engineContext(cwd: string, sessionId?: string): Context {
	const sessionsDir = getSessionsDir();
	const context: Context = { sessionsDir, hereDir: sessionDirForCwd(cwd, sessionsDir), cacheDir: cacheDir(), cwd };
	return sessionId === undefined ? context : { ...context, sessionId };
}
