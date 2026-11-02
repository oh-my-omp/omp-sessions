// Language of human-facing text (tool labels). Resolution order, per the oh-my-omp convention:
// OMOMP_LANG → the agent-dir omomp file's `omomp-lang` section → LC_ALL / LC_MESSAGES / LANG → English.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { z } from "@oh-my-pi/pi-coding-agent";
import { getAgentDir } from "@oh-my-pi/pi-utils";

export type Language = "en" | "zh";

const CONFIG_FILES = ["omomp.yml", "omomp.yaml", "omomp.json", "omomp.toml"] as const;

/** Read once per process: undefined until read, then the configured language or null for none. */
let configured: Language | null | undefined;

/** The `omomp-lang` section must be exactly `{ language: "en" | "zh" }`; other top-level sections are ignored. */
const omompFile = z.object({ "omomp-lang": z.object({ language: z.enum(["en", "zh"]) }).strict() });

function parseConfig(path: string, text: string): unknown {
	if (path.endsWith(".json")) return JSON.parse(text);
	if (path.endsWith(".toml")) return Bun.TOML.parse(text);
	return Bun.YAML.parse(text);
}

function readConfigured(): Language | null {
	const found: { path: string; text: string }[] = [];
	for (const name of CONFIG_FILES) {
		const path = join(getAgentDir(), name);
		try {
			found.push({ path, text: readFileSync(path, "utf8") });
		} catch {
			// Missing or unreadable: this file provides nothing.
		}
	}
	// Several files are ambiguous: none of them decides.
	const [only, ...rest] = found;
	if (only === undefined || rest.length > 0) return null;
	try {
		const parsed = omompFile.safeParse(parseConfig(only.path, only.text));
		return parsed.success ? parsed.data["omomp-lang"].language : null;
	} catch {
		// Unparseable file: no configured language.
		return null;
	}
}

export function language(): Language {
	const explicit = process.env.OMOMP_LANG?.trim().toLowerCase();
	if (explicit === "en" || explicit === "zh") return explicit;
	if (configured === undefined) configured = readConfigured();
	if (configured !== null) return configured;
	for (const key of ["LC_ALL", "LC_MESSAGES", "LANG"] as const) {
		const value = process.env[key]?.trim();
		if (value) return value.toLowerCase().startsWith("zh") ? "zh" : "en";
	}
	return "en";
}

/** The text for the current language; English comes first at each use site. */
export function tr(en: string, zh: string): string {
	return language() === "zh" ? zh : en;
}
