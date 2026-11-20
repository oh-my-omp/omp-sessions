import {
	findSessionFiles,
	listRecentSessions,
	readSessionInfo,
	resolveSessionLeafId,
	type SessionEntry,
	type SessionEntryBase,
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

export function listSessions(sessionDir: string | undefined, limit: number): Promise<SessionInfo[]> {
	return listRecentSessions({ limit, sessionDir });
}

export function latestRecaps(sessionIds: readonly string[]): Map<string, string> {
	const latest = new Map<string, string>();
	for (const row of listSessionRecaps({ sessionIds })) if (!latest.has(row.sessionId)) latest.set(row.sessionId, row.recap);
	return latest;
}

export function promptMatches(query: string, limit: number): string[] {
	return HistoryStorage.open().matchingSessionIds(query, limit);
}

/** Entry fields the index materializes; resolveSessionLeafId reads only id and activeLeafId. */
export type LeafNode = Pick<SessionEntryBase, "id" | "parentId" | "activeLeafId">;

export function activeLeaf(nodes: readonly LeafNode[]): string | null {
	return resolveSessionLeafId(nodes as SessionEntry[]);
}
