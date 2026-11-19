import * as fs from "node:fs/promises";
import * as path from "node:path";
import { type LoadedSession, loadSession } from "./events.ts";
import type { EventFilter } from "./filter.ts";
import { getSessionsDir, latestRecaps, listSessions, promptMatches, type SessionInfo, sessionDirForCwd } from "./omp-adapter.ts";
import { eventRecords } from "./render.ts";
import { childAgents, childDir, type SessionRef } from "./routes.ts";

export const ALL_SESSIONS = 1_000_000;

export interface SessionRow {
	readonly sid: string;
	readonly url: string;
	readonly timeline: string;
	readonly project: string;
	readonly cwd: string;
	readonly title: string;
	readonly created: string | null;
	readonly modified: string;
	readonly status: string | null;
	readonly assistantTurns: number | null;
	readonly size: number;
	readonly recap: string | null;
	readonly parent: string | null;
}

function titleOf(info: SessionInfo): string {
	return info.title ?? (info.firstMessage.split("\n", 1)[0] ?? "").slice(0, 200);
}

export function metadata(infos: readonly SessionInfo[]): SessionRow[] {
	const recaps = latestRecaps(infos.map((info) => info.id));
	return infos.map((info) => ({
		sid: info.id,
		url: `session://${info.id}`,
		timeline: `session://${info.id}/timeline`,
		project: path.basename(path.dirname(info.path)),
		cwd: info.cwd,
		title: titleOf(info),
		created: Number.isNaN(info.created.getTime()) ? null : info.created.toISOString(),
		modified: info.modified.toISOString(),
		status: info.status ?? null,
		assistantTurns: info.assistantTurns ?? null,
		size: info.size,
		recap: recaps.get(info.id) ?? null,
		parent: info.parentSessionPath ?? null,
	}));
}

export function projectDir(project: string | null, cwd: string | undefined): string | undefined {
	if (project === null) return undefined;
	if (project === ".") {
		if (!cwd) throw new Error("project=. needs the calling session's working directory");
		return sessionDirForCwd(cwd);
	}
	return project.includes("/") ? sessionDirForCwd(path.resolve(cwd ?? "/", project)) : path.join(getSessionsDir(), project);
}

function matchText(infos: readonly SessionInfo[], needle: string): SessionInfo[] {
	const fromPrompts = new Set(promptMatches(needle, 500));
	const words = needle.toLowerCase().split(/\s+/);
	const titled = (info: SessionInfo): boolean => {
		const title = titleOf(info).toLowerCase();
		return words.every((word) => title.includes(word));
	};
	const open = infos.filter((info) => !fromPrompts.has(info.id) && !titled(info));
	const recaps = latestRecaps(open.map((info) => info.id));
	return infos.filter((info) => {
		if (fromPrompts.has(info.id) || titled(info)) return true;
		const haystack = `${titleOf(info)}\n${recaps.get(info.id) ?? ""}`.toLowerCase();
		return words.every((word) => haystack.includes(word));
	});
}

export interface SessionQuery {
	readonly project: string | null;
	readonly text: string | null;
	readonly since: number | null;
	readonly until: number | null;
	readonly limit: number;
}

export async function sessionRows(query: SessionQuery, cwd: string | undefined): Promise<SessionRow[]> {
	const unfiltered = query.since === null && query.until === null && query.text === null;
	const infos = (await listSessions(projectDir(query.project, cwd), unfiltered ? query.limit : ALL_SESSIONS)).filter((info) => {
		const modified = info.modified.getTime();
		return (query.since === null || modified >= query.since) && (query.until === null || modified < query.until);
	});
	return metadata((query.text === null ? infos : matchText(infos, query.text)).slice(0, query.limit));
}

export interface TranscriptRow {
	readonly sid: string;
	readonly agent: string;
	readonly parent: string | null;
	readonly file: string;
	readonly bytes: number;
	readonly modified: string;
}

export async function transcriptRow(sid: string, agent: string, parent: string | null, file: string): Promise<TranscriptRow> {
	const stat = await fs.stat(file);
	return { sid, agent, parent, file, bytes: stat.size, modified: stat.mtime.toISOString() };
}

export async function transcriptTree(ref: SessionRef): Promise<TranscriptRow[]> {
	const rows: TranscriptRow[] = [await transcriptRow(ref.sid, "", null, ref.file)];
	for (let index = 0; index < rows.length; index++) {
		const row = rows[index];
		if (!row) break;
		for (const agent of await childAgents(row.file))
			rows.push(await transcriptRow(ref.sid, agent, row.agent, path.join(childDir(row.file), `${agent}.jsonl`)));
	}
	return rows;
}

export function transcriptUrl(sid: string, agent: string): string {
	return agent === "" ? `session://${sid}` : `session://${sid}/agents/${agent}`;
}

export type EventRecord = Record<string, unknown>;

export async function transcriptEvents(
	transcript: TranscriptRow,
	filter: EventFilter,
	signal: AbortSignal | undefined,
): Promise<{ readonly loaded: LoadedSession; readonly records: EventRecord[] }> {
	const loaded = await loadSession(transcript.file, transcript.sid, transcript.agent, signal);
	return { loaded, records: [...eventRecords(loaded, transcriptUrl(transcript.sid, transcript.agent), filter)] };
}
