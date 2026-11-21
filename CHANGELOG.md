# Changelog

## [Unreleased]

### Breaking Changes

- `session_query` no longer has the `sessions` action or its `project`, `text`, `since` and `until` parameters; a `sessions` call fails with `bad_request` naming omp's `archive` eval global, which lists sessions, searches prompts and reads recaps
- `session://find`, `session://README` and the three `sessions.jsonl` URLs are gone, and `src.sessions` has no `recap` column; the `session://` grammar is now at `xd://read/urls`

### Changed

- `session://` reads and `session_query` calls run in a separate engine process (Rust, `engine/`) instead of inside omp, so omp's memory no longer grows with the sessions a query reads; `sql` and `export` run in one query process per omp session, which keeps the tables created in `main` until `/new` or exit
- The engine binary is downloaded from the GitHub Release of the installed version on first use and checked against the sha256 that Release's `digests.json` lists; the DuckDB native addon (`@duckdb/node-api`) is no longer installed
- A transcript whose last message is more than 24 hours old is parsed once and kept under omp's agent cache directory (`omp-sessions`), so repeated reads and queries of it skip the session file; transcripts still being written are read from the file every time
- Failures name their kind: `session://: <kind>: <message>` for reads and `session_query <action> failed: <kind>: <message>` for tool calls, with kinds such as `not_found`, `query_failed`, `aborted`, `engine_exited` and `engine_unavailable`

## [0.1.23] - 2026-11-20

### Fixed

- A sid-prefix lookup now also matches header-named session files when a stamped name shares the prefix, and distinct ids from stamped names and headers make the prefix ambiguous
- A session file whose name carries no timestamp (one kept under its own name, such as `current.jsonl`, after `omp --resume <path>`) is now reachable: its date directory, its date URL and its sid-prefix URL resolve it, taking the id and creation date from its header. Such a file was listed in `sessions.jsonl` but `session://<sid>` reported it missing. Stamped names keep their id and date from the name, and a stamped file still wins a shared id; a date directory lists an id shared by several files of that day once, and its URL opens the stamped file, or without one the file whose name sorts first.

## [0.1.22] - 2026-11-19

### Changed

- Updated the shared copy of `i18n.ts` to oh-my-omp/shared@33dbe00.

## [0.1.21] - 2026-11-15

### Security

- `session_query` `export` now refuses a target that reaches the omp sessions directory under another spelling (different letter case, a Windows 8.3 short name) or through a symbolic link; such a target used to be written into the sessions directory

## [0.1.20] - 2026-11-12

### Changed

- omp-sessions publishes from a daily release instead of a patch version on every push to `main`: installs come from the `release` branch (`https://github.com/oh-my-omp/omp-sessions#release`), and the package declares `omomp.changelog` and ships `CHANGELOG.md`, so the merged What's New shows what changed here.
