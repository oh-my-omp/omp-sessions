// Developer entry for the Rust engine in engine/ (see AGENTS.md).
//   bun scripts/engine.ts build   release build of this platform's binary into engine/bin/
//   bun scripts/engine.ts check   clippy, and the committed protocol types against a fresh generation
//   bun scripts/engine.ts test    cargo test
import { copyFileSync, mkdirSync, readFileSync } from "node:fs";
import * as path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const manifest = path.join(root, "engine", "Cargo.toml");
const binary = `omp-sessions-engine${process.platform === "win32" ? ".exe" : ""}`;
/** The generated protocol types the shell imports. */
export const PROTOCOL_TS = path.join(root, "src", "engine", "protocol.ts");

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

async function protocol(): Promise<void> {
	await Bun.write(PROTOCOL_TS, cargoOutput(["run", "--quiet", "--features", "ts", "--bin", "protocol-ts"]));
	process.stdout.write(`${path.relative(root, PROTOCOL_TS)}\n`);
}

const command = process.argv[2];
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
	default:
		process.stderr.write("usage: bun scripts/engine.ts build|check|protocol|test\n");
		process.exit(2);
}
