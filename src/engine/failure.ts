// The error kinds a call can end with (decision §15): the engine's, carried in
// its error frames, and the two only the shell can observe.
import type { ErrorKind } from "./protocol.ts";

/** Every kind a `session://` read or a `session_query` call can fail with. */
export type FailureKind = ErrorKind | "engine_unavailable" | "engine_exited";

/** Why the shell has no engine binary to run (decision §18). */
export type UnavailableReason = "unsupported_platform" | "download_failed" | "digest_mismatch" | "not_built";

export class EngineFailure extends Error {
	constructor(
		readonly kind: FailureKind,
		message: string,
	) {
		super(message);
		this.name = "EngineFailure";
	}

	/** `<kind>: <message>`, the text both entry points put after their own prefix. */
	get text(): string {
		return `${this.kind}: ${this.message}`;
	}
}

export function unavailable(reason: UnavailableReason, detail: string): EngineFailure {
	return new EngineFailure("engine_unavailable", `${reason}: ${detail}`);
}

export function aborted(detail: string): EngineFailure {
	return new EngineFailure("aborted", detail);
}

/** A failure's `<kind>: <message>`; anything else is a bug in the shell, shown as its message. */
export function failureText(error: unknown): string {
	if (error instanceof EngineFailure) return error.text;
	return error instanceof Error ? error.message : String(error);
}
