# omp-sessions

An omp plugin: the TypeScript shell in `index.ts` and `src/`, and the Rust engine in `engine/` that reads the session files.

## Entry points

| Command | What it runs |
|---|---|
| `bun run engine:build` | release build of this platform's engine into `engine/bin/omp-sessions-engine` (`.exe` on Windows) |
| `bun run check` | Biome, `tsc`, `cargo clippy -D warnings`, and the protocol drift check |
| `bun run test` | `bun test`, then `cargo test` |
| `bun scripts/engine.ts protocol` | regenerates `src/engine/protocol.ts` from `engine/src/protocol.rs` |
| `bun scripts/engine.ts artifact <target> <dir>` | builds this machine's engine and writes it as `omp-sessions-engine-<target>` (`.exe` on Windows) into `<dir>`; `<target>` must be this machine's own platform |
| `bun scripts/engine.ts plan` | prints `tag=v<version>` when the GitHub Release of `package.json`'s version exists without `digests.json`, and nothing otherwise |
| `bun scripts/engine.ts publish <tag> <dir>` | uploads the six engine assets in `<dir>` to Release `<tag>`, then the `digests.json` listing their sha256; uploads nothing when one is missing |
| `bun scripts/engine.ts smoke <binary>` | has an engine binary answer one `session://` timeline read over a one-session fixture; run it by hand on a machine of the binary's platform, for example on an asset downloaded from a Release |

`.github/workflows/engine-assets.yml` runs `plan`, `artifact` and `publish` after each release (the `engine` job of `omomp-release.yml`). The shell downloads a missing engine binary from the Release of its own version and checks it against that Release's `digests.json`.

`src/engine/protocol.ts` is generated from `engine/src/protocol.rs`; `bun run check` fails when the committed file differs from a fresh generation.
