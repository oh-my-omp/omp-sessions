// Every omp-internal import this package depends on, in one place. Symbols the
// host exports from its package root or its `internal-urls` barrel come from
// there; the rest are host subpaths (no plugin API contract). An omp upgrade
// that moves a subpath module fails here at load, which omp reports as an
// extension load error.
import {
	findSessionFiles,
	listRecentSessions,
	readSessionInfo,
	resolveSessionLeafId,
	type SessionEntry,
	type SessionInfo,
} from "@oh-my-pi/pi-coding-agent";
import { InternalUrlRouter } from "@oh-my-pi/pi-coding-agent/internal-urls";
import { HistoryStorage } from "@oh-my-pi/pi-coding-agent/session/history-storage";
import { listSessionRecaps } from "@oh-my-pi/pi-coding-agent/session/session-index";
import { sessionDirForCwd } from "@oh-my-pi/pi-coding-agent/session/session-paths";
import { executeJsonQuery, type JsonSelector, parseJsonSelector } from "@oh-my-pi/pi-coding-agent/tools/read-json";
import { getSessionsDir } from "@oh-my-pi/pi-utils";

export type {
	InternalResource,
	InternalUrl,
	ProtocolHandler,
	ResolveContext,
	SchemeSpec,
} from "@oh-my-pi/pi-coding-agent/internal-urls";
export { type ShellFsDirEntry, ShellFsFileType } from "@oh-my-pi/pi-natives";
export type { JsonSelector, SessionInfo };

export { executeJsonQuery, findSessionFiles, getSessionsDir, parseJsonSelector, readSessionInfo, sessionDirForCwd };

export function router(): InternalUrlRouter {
	return InternalUrlRouter.instance();
}

/** Newest-first sessions of one project directory, or of every project when `sessionDir` is omitted. */
export function listSessions(sessionDir: string | undefined, limit: number): Promise<SessionInfo[]> {
	return listRecentSessions({ limit, sessionDir });
}

/** Newest recap per session id, from OMP's recap journal. */
export function latestRecaps(sessionIds: readonly string[]): Map<string, string> {
	const latest = new Map<string, string>();
	for (const row of listSessionRecaps({ sessionIds })) if (!latest.has(row.sessionId)) latest.set(row.sessionId, row.recap);
	return latest;
}

/** Session ids whose interactive prompts match `query` in OMP's prompt-history FTS index. */
export function promptMatches(query: string, limit: number): string[] {
	return HistoryStorage.open().matchingSessionIds(query, limit);
}

/** A journal node as OMP's leaf replay reads it. */
export interface LeafNode {
	readonly id: string;
	readonly activeLeafId?: string | null;
}

/** Active leaf of a journal in file order, by OMP's own replay rule. */
export function activeLeaf(nodes: readonly LeafNode[]): string | null {
	// resolveSessionLeafId reads only `id` and `activeLeafId` (session-entries.ts sessionEntryLeafId / lastSelfSelectedLeafId).
	return resolveSessionLeafId(nodes as unknown as SessionEntry[]);
}
