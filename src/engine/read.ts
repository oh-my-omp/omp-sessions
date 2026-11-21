// Read processes (decision §2): every `session://` resolve, readDirectory and
// locate, and every `agents` and `events` call, runs one engine process. Its
// start anchor is the call; its end anchor is the response read with the
// process exited. The shell writes one request frame, closes stdin and reads
// one response frame. The call's AbortSignal terminates the process tree and
// the call ends `aborted`.
import { ptree } from "@oh-my-pi/pi-utils";
import { engineBinary } from "./binary.ts";
import { aborted, EngineFailure } from "./failure.ts";
import { forwardLog, type Logger, parseResponse, replyOf, requestFrame } from "./frames.ts";
import type { Context, Op, Reply } from "./protocol.ts";

/** The ops a read process answers; `sql` and `export` belong to the query process. */
export type ReadOp = Extract<Op, { op: "resolve" | "readDirectory" | "locate" | "agents" | "events" }>;

/** How a process ended, for `engine_exited` messages. */
export function exitDescription(exitCode: number | null, signalCode: NodeJS.Signals | number | null): string {
	if (signalCode !== null) return `signal ${signalCode}`;
	return exitCode === null ? "no exit status" : `exit code ${exitCode}`;
}

export async function readRequest(op: ReadOp, context: Context, signal: AbortSignal | undefined, logger: Logger): Promise<Reply> {
	if (signal?.aborted) throw aborted(`the ${op.op} call was cancelled before it started`);
	const binary = await engineBinary(signal);
	const child = ptree.spawn([binary, "read"], { stdin: "pipe", signal });
	child.stdin.write(requestFrame(1, context, op));
	child.stdin.end();
	const result = await child.wait({ allowNonZero: true, allowAbort: true });
	forwardLog(logger, result.stderr);
	if (result.exitError?.aborted === true || signal?.aborted === true) {
		throw aborted(`the ${op.op} call was cancelled; its engine process tree was terminated`);
	}
	const line = result.stdout.split("\n").find((candidate) => candidate.trim() !== "");
	if (line === undefined) {
		throw new EngineFailure(
			"engine_exited",
			`the read process exited before responding (${exitDescription(child.exitCode, child.proc.signalCode)})`,
		);
	}
	return replyOf(parseResponse(line));
}
