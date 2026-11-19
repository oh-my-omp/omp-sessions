# omp-sessions

An omp plugin: the TypeScript shell in `index.ts` and `src/`, and the Rust engine in `engine/` that reads the session files.

## Entry points

| Command | What it runs |
|---|---|
| `bun run engine:build` | release build of this platform's engine into `engine/bin/omp-sessions-engine` (`.exe` on Windows) |
| `bun run check` | Biome, `tsc`, `cargo clippy -D warnings`, and the protocol drift check |
| `bun run test` | `bun test`, then `cargo test` |
| `bun scripts/engine.ts protocol` | regenerates `src/engine/protocol.ts` from `engine/src/protocol.rs` |

`src/engine/protocol.ts` is generated from `engine/src/protocol.rs`; `bun run check` fails when the committed file differs from a fresh generation.
