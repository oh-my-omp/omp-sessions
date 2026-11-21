// Developer entry for the Rust engine in engine/ (see AGENTS.md).
//   bun scripts/engine.ts build   release build of this platform's binary into engine/bin/
//   bun scripts/engine.ts check   clippy, and the committed protocol types against a fresh generation
//   bun scripts/engine.ts test    cargo test
//   bun scripts/engine.ts artifact <target> <dir>   build <target>'s release asset into <dir>
//   bun scripts/engine.ts plan                      print `tag=v<version>` when that Release lacks digests.json
//   bun scripts/engine.ts publish <tag> <dir>       upload the six assets in <dir>, then their digests.json
//   bun scripts/engine.ts smoke <binary>            have an engine binary answer one session:// read (run by hand)
// artifact, plan and publish are the jobs of .github/workflows/engine-assets.yml (decision §19).
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Request, Response } from "../src/engine/protocol.ts";

const root = path.resolve(import.meta.dir, "..");
const manifest = path.join(root, "engine", "Cargo.toml");
const binary = `omp-sessions-engine${process.platform === "win32" ? ".exe" : ""}`;
/** The generated protocol types the shell imports. */
export const PROTOCOL_TS = path.join(root, "src", "engine", "protocol.ts");

const command = process.argv[2];
const USAGE = "bun scripts/engine.ts build|check|protocol|test | artifact <target> <dir> | plan | publish <tag> <dir> | smoke <binary>";

function exitOnFailure(args: readonly string[], exitCode: number | null): void {
	if (exitCode === 0) return;
	process.stderr.write(`cargo ${args.join(" ")} failed with exit code ${exitCode}\n`);
	process.exit(exitCode ?? 1);
}

/** `cargo <subcommand> --manifest-path engine/Cargo.toml <rest>`: the path goes before a `--` that `rest` may carry. */
function argv([subcommand = "", ...rest]: readonly string[]): string[] {
	return ["cargo", subcommand, "--manifest-path", manifest, ...rest];
}

function cargo(args: readonly string[]): void {
	const run = Bun.spawnSync(argv(args), { cwd: root, stdout: "inherit", stderr: "inherit" });
	exitOnFailure(args, run.exitCode);
}

function cargoOutput(args: readonly string[]): string {
	const run = Bun.spawnSync(argv(args), { cwd: root, stdout: "pipe", stderr: "inherit" });
	exitOnFailure(args, run.exitCode);
	return run.stdout.toString();
}

function build(): void {
	cargo(["build", "--release", "--bin", "omp-sessions-engine"]);
	const target = path.join(root, "engine", "bin");
	mkdirSync(target, { recursive: true });
	copyFileSync(path.join(root, "engine", "target", "release", binary), path.join(target, binary));
	process.stdout.write(`engine/bin/${binary}\n`);
}

function check(): void {
	cargo(["clippy", "--all-targets", "--features", "ts", "--quiet", "--", "-D", "warnings"]);
	const fresh = cargoOutput(["run", "--quiet", "--features", "ts", "--bin", "protocol-ts"]);
	const committed = readFileSync(PROTOCOL_TS, "utf8");
	if (fresh !== committed) {
		process.stderr.write(
			`${path.relative(root, PROTOCOL_TS)} differs from the types in engine/src/protocol.rs; regenerate it with \`bun scripts/engine.ts protocol\`.\n`,
		);
		process.exit(1);
	}
}

/** The release targets (decision §18), named `<process.platform>-<process.arch>`. */
const TARGETS = ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64", "win32-x64", "win32-arm64"] as const;
type Target = (typeof TARGETS)[number];

/** The Release asset that lists the sha256 of every engine asset (decision §18). */
const DIGESTS = "digests.json";

function fail(message: string): never {
	process.stderr.write(`scripts/engine.ts ${command}: ${message}\n`);
	process.exit(1);
}

/** The Release asset name of `target`'s engine. */
function assetName(target: Target): string {
	return `omp-sessions-engine-${target}${target.startsWith("win32-") ? ".exe" : ""}`;
}

/** A command-line argument that must be present. */
function operand(index: number, name: string): string {
	const value = process.argv[index] ?? "";
	if (value === "") fail(`missing <${name}>; usage: ${USAGE}`);
	return value;
}

/** `name` as a target, which must be this machine's own platform: each asset is built natively on its own platform. */
function artifactTarget(name: string): Target {
	const target = TARGETS.find((candidate) => candidate === name);
	if (target === undefined) fail(`target "${name}" is not one of ${TARGETS.join(", ")}`);
	const host = `${process.platform}-${process.arch}`;
	if (host !== target) fail(`target ${target} is built on a ${target} machine; this one is ${host}`);
	return target;
}

function sha256(file: string): string {
	return new Bun.CryptoHasher("sha256").update(readFileSync(file)).digest("hex");
}

function gh(args: readonly string[]): void {
	const run = Bun.spawnSync(["gh", ...args], { cwd: root, stdout: "inherit", stderr: "pipe" });
	if (run.exitCode !== 0) fail(`gh ${args.join(" ")} exited ${run.exitCode}: ${run.stderr.toString().trim()}`);
}

const SMOKE_SID = "01ff0000-0000-7000-8000-000000000057";
const SMOKE_TEXT = "omp-sessions engine release smoke";

/**
 * Sends one `resolve session://<sid>/timeline` request over a one-session sessions directory to
 * `engine read`, and fails unless the answer is the timeline holding the session's user message.
 */
function smoke(engine: string, protocolVersion: number): void {
	const href = `session://${SMOKE_SID}/timeline`;
	const run = smokeRead(engine, protocolVersion, href);
	const answer = run.stdout.toString().split("\n")[0] ?? "";
	if (run.exitCode !== 0) fail(`${engine} read exited ${run.exitCode}: ${run.stderr.toString()}`);
	let response: Response;
	try {
		response = JSON.parse(answer);
	} catch {
		fail(`${engine} read answered no response frame: ${JSON.stringify(answer.slice(0, 500))}`);
	}
	if (!response.ok) fail(`${href} answered ${response.error.kind}: ${response.error.message}`);
	if (response.result.kind !== "resource" || !response.result.content.includes(SMOKE_TEXT)) {
		fail(`${href} answered without the session's message: ${answer.slice(0, 500)}`);
	}
	process.stdout.write(
		`${engine} read answered ${href} with a ${response.result.content.length}-character timeline holding "${SMOKE_TEXT}"\n`,
	);
}

/** Runs `engine read` on the request for `href` over a one-session sessions directory that is removed afterwards. */
function smokeRead(engine: string, protocolVersion: number, href: string): Bun.SyncSubprocess<"pipe", "pipe"> {
	const sessionsDir = mkdtempSync(path.join(os.tmpdir(), "omp-sessions-smoke-"));
	try {
		const bucket = path.join(sessionsDir, "--smoke--");
		mkdirSync(bucket);
		const header = { type: "session", version: 3, id: SMOKE_SID, timestamp: "2026-01-01T00:00:00.000Z", cwd: "/smoke" };
		const message = {
			type: "message",
			id: "a0000001",
			parentId: null,
			timestamp: "2026-01-01T00:00:01.000Z",
			message: { role: "user", content: [{ type: "text", text: SMOKE_TEXT }], timestamp: 1767225601000 },
		};
		writeFileSync(
			path.join(bucket, `2026-01-01T00-00-00-000Z_${SMOKE_SID}.jsonl`),
			`${JSON.stringify(header)}\n${JSON.stringify(message)}\n`,
		);
		const request: Request = {
			id: 1,
			protocol: protocolVersion,
			context: { sessionsDir, hereDir: bucket, cacheDir: "", cwd: sessionsDir },
			op: "resolve",
			url: { href, host: SMOKE_SID, pathname: "/timeline", search: "" },
			skipDirectoryListing: false,
		};
		return Bun.spawnSync([engine, "read"], { stdin: Buffer.from(`${JSON.stringify(request)}\n`), stdout: "pipe", stderr: "pipe" });
	} finally {
		rmSync(sessionsDir, { recursive: true, force: true });
	}
}

/** Builds this machine's engine and writes it as `omp-sessions-engine-<target>` (`.exe` on win32) into `dir`. */
function artifact(name: string, dir: string): void {
	const target = artifactTarget(name);
	cargo(["build", "--release", "--locked", "--bin", "omp-sessions-engine"]);
	const built = path.join(root, "engine", "target", "release", binary);
	const asset = assetName(target);
	mkdirSync(dir, { recursive: true });
	copyFileSync(built, path.join(dir, asset));
	process.stdout.write(`${asset} ${sha256(path.join(dir, asset))}\n`);
}

/** Prints `tag=v<package.json version>` when that GitHub Release exists without digests.json, and nothing otherwise. */
function plan(): void {
	const manifest: unknown = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));
	const version = typeof manifest === "object" && manifest !== null && "version" in manifest ? manifest.version : undefined;
	if (typeof version !== "string" || version === "") fail("package.json has no version");
	const tag = `v${version}`;
	const view = Bun.spawnSync(["gh", "release", "view", tag, "--json", "assets", "--jq", ".assets[].name"], {
		cwd: root,
		stdout: "pipe",
		stderr: "pipe",
	});
	if (view.exitCode !== 0) {
		const error = view.stderr.toString().trim();
		if (error !== "release not found") fail(`gh release view ${tag} exited ${view.exitCode}: ${error}`);
		process.stderr.write(`${tag} has no GitHub Release; nothing to build\n`);
		return;
	}
	if (view.stdout.toString().split("\n").includes(DIGESTS)) {
		process.stderr.write(`${tag} already has ${DIGESTS}; nothing to build\n`);
		return;
	}
	process.stdout.write(`tag=${tag}\n`);
}

/** Uploads the six engine assets in `dir` to Release `tag`, then the digests.json that lists their sha256. */
function publish(tag: string, dir: string): void {
	const assets = TARGETS.map(assetName);
	const missing = assets.filter((asset) => !existsSync(path.join(dir, asset)));
	if (missing.length > 0) fail(`${dir} lacks ${missing.join(", ")}; nothing was uploaded`);
	const sums = Object.fromEntries(assets.map((asset) => [asset, sha256(path.join(dir, asset))]));
	const digests = path.join(dir, DIGESTS);
	writeFileSync(digests, `${JSON.stringify(sums, null, "\t")}\n`);
	gh(["release", "upload", tag, ...assets.map((asset) => path.join(dir, asset)), "--clobber"]);
	gh(["release", "upload", tag, digests, "--clobber"]);
	for (const asset of assets) process.stdout.write(`${tag} ${asset} ${sums[asset]}\n`);
	process.stdout.write(`${tag} ${DIGESTS}\n`);
}

async function protocol(): Promise<void> {
	await Bun.write(PROTOCOL_TS, cargoOutput(["run", "--quiet", "--features", "ts", "--bin", "protocol-ts"]));
	process.stdout.write(`${path.relative(root, PROTOCOL_TS)}\n`);
}

switch (command) {
	case "build":
		build();
		break;
	case "check":
		check();
		break;
	case "protocol":
		await protocol();
		break;
	case "test":
		cargo(["test", "--quiet"]);
		break;
	case "artifact":
		artifact(operand(3, "target"), operand(4, "dir"));
		break;
	case "smoke": {
		// Loaded here, not at the top: `protocol` regenerates this file and must run while it is broken.
		const { PROTOCOL } = await import("../src/engine/protocol.ts");
		smoke(path.resolve(operand(3, "binary")), PROTOCOL);
		break;
	}
	case "plan":
		plan();
		break;
	case "publish":
		publish(operand(3, "tag"), operand(4, "dir"));
		break;
	default:
		process.stderr.write(`usage: ${USAGE}\n`);
		process.exit(2);
}
