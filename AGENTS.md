# omp-sessions

An omp plugin: the TypeScript shell in `index.ts` and `src/`, and the Rust engine in `engine/` that reads the session files.

## Entry points

| Command | What it runs |
|---|---|
| `bun run engine:build` | release build of this platform's engine into `engine/bin/omp-sessions-engine` (`.exe` on Windows) |
| `bun run check` | Biome, `tsc`, `cargo clippy -D warnings`, and the protocol drift check |
| `bun run test` | `bun test`, then `cargo test` |
| `bun scripts/engine.ts protocol` | regenerates `src/engine/protocol.ts` from `engine/src/protocol.rs` |

The wire protocol is defined only in `engine/src/protocol.rs`. `src/engine/protocol.ts` is generated from it and committed; `bun run check` fails when the two differ, so regenerate after changing a protocol type and never edit the TypeScript file by hand.

The `session://` grammar text is `engine/url-grammar.md`; it changes together with `engine/src/routes.rs`.

`engine/target/` and `engine/bin/` are build output and stay out of git. `engine/Cargo.lock` is committed.
