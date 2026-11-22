# SQL over the session files

`session_query` runs DuckDB SQL in the calling omp session's query process: an engine process outside omp, started by the session's first `session_query` call, holding an in-memory database that belongs to that session.

## Schema `src`: this call's relations

Built when the call starts from `from` / `sids` / `agents` and `tables`, dropped when it ends. Pass `from: "here"` (current project) or `from: "all"` (all projects), or explicit `sids: [...]`; omit both to query only `main.*` or `tables`. Pass `agents: []` for root transcripts only. A SELECT whose parse tree names `src.<name>` — quotes, comments, whitespace and case included — builds exactly those relations. The call builds every relation when the parse tree names a table function (`query()`, `query_table()`, `pragma_table_info()`, …) or a `SHOW`/`DESCRIBE`/`SUMMARIZE`, when the statement is not a SELECT (`CREATE TABLE … AS`), when DuckDB cannot parse or serialize it, or when a stored view or macro definition mentions `src` or one of those run-time names. A view over `src` stops working after the call; keep results with `CREATE TABLE … AS`.

- `src.files`(sid, agent, parent, file, bytes, modified): each transcript file read: the root (agent `""`) and, unless `agents: []`, every subagent with the allocated id it ran under as `parent`.
- `src.entries`(file, json, type, id, parent_id, t): every line of those files, read in place from disk when the query runs. `json` is the whole record (`json->>'$.message.role'`, `json_extract`, `unnest(json->'$.message.content')`); join to `src.files` on `file`. All branches, not only the active one. A line still being written fails the query.
- `src.sessions`(sid, project, cwd, title, created, modified, status, assistant_turns, size, parent, url): each named session's metadata from the head and tail of its file. `parent` is the header's parentSession as written: a file path (forks, task children) or a session id (continuations), not a key. Finding sessions and prompt search live in omp's `archive` eval global.
- `src.transcripts`(sid, agent, parent, title, created, parent_session, entries, on_branch, events, url): each transcript with its active-branch size.
- `src.events`(sid, agent, seq, turn, t, kind, entry, block, line, call_id, tool, status, result_entry, text, data, url): the active branch in order, as `events.jsonl` serves it; `data` is the record as JSON without the field that went into `text` (its `text`, or its `source` when it has no `text`). A call joins to its result through `result_entry` within the same (sid, agent). Text over the session:// limits ends in `…[clipped: N chars total]`; `url` reads the entry in full.
- `src.<name>`: each `{ name, sids?, agent? }` entry in `tables` materialized from the corresponding transcript events.

## Schema `main`: results that last

Tables, views and macros created without a schema (`CREATE TABLE hot AS SELECT …`) stay for the rest of this omp session and every later `session_query` call sees them. `/new`, resume, fork or ending the session end the query process and discard them; so does the process exiting on its own, which the call that meets it reports as `engine_exited`.

The session's database holds at most 4 GB in memory. Past that, tables and the joins, sorts and aggregates of a query spill to a private temporary directory that is removed when the query process ends; a query that needs a single allocation larger than what is left fails with DuckDB's `Out of Memory Error`. That memory belongs to the query process, not to omp.

## Result size

A query without `export_to` returns rows as JSON objects, at most 1000 of them by default and within a character budget of 16 MiB. The budget counts each row's values without serializing the row: a string by its length, a number, boolean or null by its JSON text, and a list or struct by its values recursively. A row that would not fit the remaining budget is not returned, and `truncated` becomes `true`; every row that is returned is exactly the row the same query returns when it fits. `truncated` also reports a row that exists past the row cap.

## Export (`export_to`)

Setting `export_to: { path, format }` reads the whole query result and writes it with DuckDB COPY: `parquet`, `csv` (one header line) or `json` (one object per line). It writes the file from a separate engine that runs no caller SQL, so the export engine touches only the file you name (requires `write` approval). When the call is aborted while the caller's result is still being read, the call fails and no file is created; once that read has finished, COPY runs on the export engine outside the caller's signal, so it finishes and the call returns its row count even if the signal has aborted. An abort never leaves a partial file.

## What SQL may do

- Admitted: SELECT, INSERT, UPDATE, DELETE, MERGE, CREATE, ALTER, DROP, EXPLAIN. Several statements may run in one call (`CREATE TABLE t AS …; SELECT …`); the last one's result comes back. `params` bind a single statement only (named `$key` placeholders).
- Refused: ATTACH, INSTALL, LOAD, SET, CALL, PRAGMAs that change settings, and other engine statements; COPY and EXPORT fail with DuckDB's `Permission Error`. File reads work only below the omp sessions directory (`read_json('<session file>', …)`); nothing can be written except via `export_to`.

```js
const sids = ["019a"];
const { details } = await tool.session_query({
  sids,
  query: `
    SELECT e.tool, count(*) calls, count(*) FILTER (WHERE e.status = 'error') errors
    FROM src.events e JOIN src.files f USING (sid, agent)
    WHERE e.kind = 'call' GROUP BY e.tool ORDER BY calls DESC
  `,
});
await tool.session_query({ sids, query: "CREATE TABLE errors AS SELECT * FROM src.events WHERE status = 'error'" });
await tool.session_query({
  query: "SELECT * FROM errors",
  export_to: { path: "/tmp/errors.parquet", format: "parquet" },
});
```
