# SQL over the session files

`sql` and `export` run DuckDB SQL inside omp, against an in-memory database that belongs to the calling session.

## Schema `src`: this call's relations

Built when the call starts from `from` and `tables`, dropped when it ends. Only the relations the query reaches exist: those it names as `src.<name>`, and — when it goes through the catalog (`duckdb_tables()`, `information_schema`, `pg_class`, `sqlite_master`, `SHOW`, `DESCRIBE`, `SUMMARIZE`) or a dynamic table function (`query()`, `query_table()`) — all of them. A view over `src` stops working after the call; keep results with `CREATE TABLE … AS`.

- `src.files`(sid, agent, parent, file, bytes, modified): each transcript file read: the root (agent `""`) and, with `agents`, every subagent with the allocated id it ran under as `parent`.
- `src.entries`(file, json, type, id, parent_id, t): every line of those files, read in place from disk when the query runs. `json` is the whole record (`json->>'$.message.role'`, `json_extract`, `unnest(json->'$.message.content')`); join to `src.files` on `file`. All branches, not only the active one. A line still being written fails the query.
- `src.sessions`(sid PK, project, cwd, title, created, modified, status, assistant_turns, size, recap, parent, url): the `sessions` action's row of each named session. `parent` is the header's parentSession as written: a file path (forks, task children) or a session id (continuations), not a key.
- `src.transcripts`(sid, agent, parent, title, created, parent_session, entries, on_branch, events, url): each transcript with its active-branch size.
- `src.events`(sid, agent, seq, turn, t, kind, entry, block, line, call_id, tool, status, result_entry, text, data, url): the active branch in order, as `events.jsonl` serves it; `data` is the record as JSON without the field that went into `text` (its `text`, or its `source` when it has no `text`). A call joins to its result through `result_entry` within the same (sid, agent). Text over the session:// limits ends in `…[clipped: N chars total]`; `url` reads the entry in full.
- `src.<name>`: the rows passed in `tables`.

## Schema `main`: results that last

Tables, views and macros created without a schema (`CREATE TABLE hot AS SELECT …`) stay for the rest of this omp session and every later `sql` call sees them. `/new`, resume, fork or ending the session discard them.

The session's database holds at most 4 GB in memory. Past that, tables and the joins, sorts and aggregates of a query spill to a private temporary directory that is removed when the database closes; a query that needs a single allocation larger than what is left fails with DuckDB's `Out of Memory Error`.

## Result size

A `sql` call returns rows as JSON objects, at most `limit` of them (default 1000) and within a character budget of 16 MiB. The budget counts each row's values without serializing the row: a string by its length, a number, boolean or null by its JSON text, and a list or struct by its values recursively. A row that would not fit the remaining budget is not returned, and `truncated` becomes `true`; every row that is returned is exactly the row the same query returns when it fits. `truncated` also reports a row that exists past `limit`.

## Export

`export` reads the whole query result and writes it with DuckDB COPY: `parquet`, `csv` (one header line) or `json` (one object per line). It writes the file from a separate engine that runs no caller SQL, so the export engine touches only the file you name. When the call is aborted while the caller's result is still being read, the call fails and no file is created; once that read has finished, COPY runs on the export engine outside the caller's signal, so it finishes and the call returns its row count even if the signal has aborted. An abort never leaves a partial file.

## What SQL may do

- Admitted: SELECT, INSERT, UPDATE, DELETE, MERGE, CREATE, ALTER, DROP, EXPLAIN. Several statements may run in one call (`CREATE TABLE t AS …; SELECT …`); the last one's result comes back. `params` bind a single statement only.
- Refused: COPY, EXPORT, ATTACH, INSTALL, LOAD, SET, PRAGMA, CALL and other engine or file statements. File reads work only below the omp sessions directory (`read_json('<session file>', …)`); nothing can be written. Files come out through the `export` action.

```js
const from = { sids: ["019a"] };
const { details } = await tool.session_query({ action: "sql", from, query: `
  SELECT e.tool, count(*) calls, count(*) FILTER (WHERE e.status = 'error') errors
  FROM src.events e JOIN src.files f USING (sid, agent)
  WHERE e.kind = 'call' GROUP BY e.tool ORDER BY calls DESC` });
await tool.session_query({ action: "sql", from, query: "CREATE TABLE errors AS SELECT * FROM src.events WHERE status = 'error'" });
await tool.session_query({ action: "export", query: "SELECT * FROM errors", path: "/tmp/errors.parquet", format: "parquet" });
```
