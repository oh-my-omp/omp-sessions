// The engine binary (decision §18): `<package root>/engine/bin/omp-sessions-engine`
// (`.exe` on win32). A file there is used as it is. Without it, the shell
// downloads `digests.json` and this platform's asset from the GitHub Release
// `v<version>` of the package's `repository.url`, checks the asset's sha256
// against digests.json, and renames it into place. A Release without
// digests.json has no engine assets (decision §19 uploads it last), so the
// binary has to be built locally with `bun run engine:build`.
// The binary lives as long as the package directory, so it always matches the
// shell's version.
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { z } from "@oh-my-pi/omptype/zod";
import { aborted, EngineFailure, unavailable } from "./failure.ts";

/** The platforms omp's own natives support (omp packages/natives/native/loader-state.js). */
const PLATFORMS = ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64", "win32-x64", "win32-arm64"] as const;
type Platform = (typeof PLATFORMS)[number];

const ROOT = path.resolve(import.meta.dir, "..", "..");
const EXE = process.platform === "win32" ? ".exe" : "";

const manifest = z.object({ version: z.string().min(1), repository: z.object({ url: z.string() }) });
const digests = z.record(z.string(), z.string().regex(/^[0-9a-f]{64}$/));

function platform(): Platform {
	const name = `${process.platform}-${process.arch}`;
	const known = PLATFORMS.find((candidate) => candidate === name);
	if (known === undefined) throw unavailable("unsupported_platform", `${name} is not one of ${PLATFORMS.join(", ")}`);
	return known;
}

/** Where the engine binary of this package lives. */
export function binaryPath(): string {
	return path.join(ROOT, "engine", "bin", `omp-sessions-engine${EXE}`);
}

async function exists(file: string): Promise<boolean> {
	return fs.stat(file).then(
		(stats) => stats.isFile(),
		() => false,
	);
}

/** The JSON text of `file`, or null when the file does not exist. */
async function jsonFile(file: string): Promise<unknown> {
	let text: string;
	try {
		text = await fs.readFile(file, "utf8");
	} catch {
		return null;
	}
	return JSON.parse(text);
}

/** `owner/name` of a GitHub repository URL as package.json writes it. */
function githubRepo(url: string): string {
	const match = /github\.com[/:]([^/]+)\/([^/]+?)(?:\.git)?\/?$/.exec(url);
	if (match === null) throw unavailable("download_failed", `package.json repository.url ${url} is not a GitHub repository`);
	return `${match[1]}/${match[2]}`;
}

/** The body of `url`; HTTP 404 throws `notFound`. */
async function download(url: string, signal: AbortSignal | undefined, notFound: EngineFailure): Promise<Uint8Array> {
	try {
		const response = await fetch(url, { signal, redirect: "follow" });
		if (response.status === 404) throw notFound;
		if (!response.ok) throw unavailable("download_failed", `${url}: HTTP ${response.status} ${response.statusText}`);
		return new Uint8Array(await response.arrayBuffer());
	} catch (error) {
		if (signal?.aborted) throw aborted(`the engine download from ${url} was cancelled`);
		if (error instanceof EngineFailure) throw error;
		throw unavailable("download_failed", `${url}: ${error instanceof Error ? error.message : String(error)}`);
	}
}

/** The engine binary to run, fetched first when the package carries none. */
export async function engineBinary(signal: AbortSignal | undefined): Promise<string> {
	const target = platform();
	const file = binaryPath();
	if (await exists(file)) return file;
	const pkg = manifest.safeParse(await jsonFile(path.join(ROOT, "package.json")));
	if (!pkg.success) throw unavailable("download_failed", `package.json: ${pkg.error.message}`);
	const tag = `v${pkg.data.version}`;
	const release = `https://github.com/${githubRepo(pkg.data.repository.url)}/releases/download/${tag}`;
	const sumsUrl = `${release}/digests.json`;
	const sumsBytes = await download(
		sumsUrl,
		signal,
		unavailable(
			"not_built",
			`${file} does not exist and Release ${tag} has no engine assets (${sumsUrl}: HTTP 404); build it with \`bun run engine:build\``,
		),
	);
	let sumsJson: unknown;
	try {
		sumsJson = JSON.parse(new TextDecoder().decode(sumsBytes));
	} catch (error) {
		throw unavailable("digest_mismatch", `${sumsUrl}: ${error instanceof Error ? error.message : String(error)}`);
	}
	const sums = digests.safeParse(sumsJson);
	if (!sums.success) throw unavailable("digest_mismatch", `${sumsUrl}: ${sums.error.message}`);
	const asset = `omp-sessions-engine-${target}${EXE}`;
	const expected = sums.data[asset];
	if (expected === undefined) throw unavailable("digest_mismatch", `${sumsUrl} lists no sha256 for ${asset}`);
	const url = `${release}/${asset}`;
	const bytes = await download(url, signal, unavailable("download_failed", `${url}: HTTP 404`));
	const actual = new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
	if (actual !== expected) throw unavailable("digest_mismatch", `${url} has sha256 ${actual}; ${sumsUrl} expects ${expected}`);
	await fs.mkdir(path.dirname(file), { recursive: true });
	const temporary = `${file}.${process.pid}.${Date.now()}.tmp`;
	try {
		await fs.writeFile(temporary, bytes, { mode: 0o755 });
		await fs.rename(temporary, file);
	} catch (error) {
		await fs.rm(temporary, { force: true });
		throw unavailable("download_failed", `writing ${file}: ${error instanceof Error ? error.message : String(error)}`);
	}
	return file;
}
