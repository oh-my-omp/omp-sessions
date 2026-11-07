# omp-sessions

English | [简体中文](README.zh-CN.md)

Reads the omp sessions on this machine (`~/.omp/agent/sessions`): the `session://` scheme lets `read` and `grep` open other sessions without locating JSONL files by hand, and the `session_query` tool lets eval cells query the same history in JavaScript and SQL.

## Install

```sh
omp install https://github.com/oh-my-omp/omp-sessions
```

The entry is the package-root `index.ts`, declared by `omp.extensions` in `package.json`. The plugin has no settings. Its SQL engine is DuckDB (`@duckdb/node-api`, a native addon installed with the plugin).

## `session://`

A read-only internal-URL scheme registered in omp's process-global URL router, so main sessions, subagents and light mode all reach it. `read session://README` returns the full grammar.

- **Find**: `session://find?text=<words>[&project=.|<bucket>|<path>][&max=N]` matches prompt history (omp's FTS index in `history.db`), titles and recaps; `session://sessions.jsonl`, `session://here/sessions.jsonl` and `session://projects/<bucket>/sessions.jsonl` list session metadata as JSONL.
- **Read**: `session://<id-prefix>/timeline` renders the active branch: user, developer, thinking, assistant text, IRC, task dispatches and subagent results in full; every other tool call is one line, `tool(args) status #<result entry> // intent`. `…/entry/<id>` shows one entry in full; `…/events.jsonl` is the same events as JSONL; `…/raw.jsonl` is the file; `…/agents/<AgentId>/…` repeats the tree for subagents; `…/artifacts/<N>` and `…/local/<path>` are that session's `artifact://` and `local://`.
- **Filters**: `timeline` and `events.jsonl` take `?type=<filter>` (event kinds, groups, `call:<tool>`, `marker:<what>`, exclusions) and `?since=…&until=…` time windows; `session://type/<filter>/<path>` is the form that survives a grep glob. jq runs on `events.jsonl`, `sessions.jsonl` and `find` with `?q=`.
- **grep**: one traversable tree, `session://projects/<bucket>/<YYYY-MM-DD>/<full-id>/timeline` (and `…/agents/<AgentId>/timeline`), aliased `session://here/…` for the current project. Each read scans the file with a projecting tokenizer (`src/scanner.ts`) through one reused read buffer: an index pass over every line picks the active branch, then a build pass reads only the branch entries the read shows; nothing is cached.

## `session_query`

A tool eval cells call as `(await tool.session_query({ action, … })).details`, in JavaScript or Python.

| action | result |
|---|---|
| `sessions` | session metadata rows, filtered by project, words and a `modified` window |
| `agents` | a session's transcript tree: the root and every subagent with the id it ran under |
| `events` | a page of one transcript's active-branch events |
| `sql` | DuckDB SQL over the sessions named in `from` and the rows passed in `tables` |
| `export` | one query result written to a parquet, csv or json file the caller names |

- **Online**: each `sql` call builds the relations of schema `src` that its query references, for the sessions it names, from the session files as they are then, and drops them when the call ends; a query that goes through the catalog (`duckdb_tables()`, `information_schema`, `SHOW`, …) or a dynamic table function (`query()`, `query_table()`) reaches every relation. `src.entries` reads every JSONL line in place on disk; `src.events`, `src.transcripts` and `src.sessions` come from the same loaders as `session://`, so they follow the active branch, complete tool calls from their results and carry recaps. No copy of the sessions is kept.
- **Intermediate results**: tables, views and macros created in schema `main` stay in that session's in-memory database for later calls; `/new`, resume, fork or ending the session discard them. The database holds at most 4 GB in memory and spills the rest to a temporary directory removed with it. Rows from a cell join a query as `src.<name>` through `tables`.
- **Limits on SQL**: queries and changes to the database's own objects run; COPY, EXPORT, ATTACH, INSTALL, LOAD, SET, PRAGMA, CALL and other engine or file statements are refused. SQL reads files only below the sessions directory and writes none; `export` writes a file, never below the sessions directory, and asks for write approval.
- Call contracts are the tool topics `xd://session_query/actions` and `xd://session_query/sql` (`src/query/topics/`).

Tool labels follow the UI language: `OMOMP_LANG` (`en` or `zh`), then `LC_ALL` / `LC_MESSAGES` / `LANG`, then English. Descriptions, topics and results stay English.

## Host coupling

Every omp-internal import (URL router, session listing, index and loader helpers, `tools/read-json`) sits in `src/omp-adapter.ts`; an omp upgrade that moves one fails there at load. The scheme handler never replaces an omp-owned `session` scheme or another host's handler.

## Development

```sh
bun install
bun run check
bun test
```

For local runs, load the entry directly: `omp -e ./index.ts`.
