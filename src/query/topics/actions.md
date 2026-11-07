# session_query actions

Call from an eval cell and read `.details`: `(await tool.session_query({ action: "sql", … })).details` in JavaScript, `(await tool.session_query({...}))["details"]` in Python. Every action reads the session files as they are when it runs. A direct call shows `details` as JSON text when it fits in 8192 characters, otherwise only `<action>: more than 8192 chars of JSON in details; call from eval and read .details`.

| action | parameters | details |
|---|---|---|
| `sessions` | `project` (`.` this project, a bucket name or a path; omitted: every project), `text`, `since`, `until` (ISO, on `modified`), `limit` (default 200) | `{rows}`: sid url timeline project cwd title created modified status assistantTurns size recap parent, newest first; every filter applies before `limit` |
| `agents` | `sid` | `{rows}`: sid agent parent file bytes modified; the root has agent "" and parent null; a subagent's parent is the allocated id it ran under |
| `events` | `sid`, `agent`, `type` (the session:// type filter), `offset`, `limit` (default 500) | `{rows, total, offset, next}`: the records of `session://<id>/events.jsonl`; `next` is the next offset or null |
| `sql` | `query`, `from` (`{sids, agents}`), `tables` (`{name: rows}`), `params`, `limit` (default 1000) | `{kind: "rows", columns, rows, truncated}` or `{kind: "changes", changes}` |
| `export` | `query` (one SELECT), `path` (absolute, outside the sessions directory), `format` (`parquet`, `csv`, `json`), `from`, `tables`, `params` | `{path, format, rows, columns}` |

- `from.sids` takes session ids or unique id prefixes (at most 50); `from.agents` (default true) also reads every subagent transcript below them. Name only the sessions the query needs: each call reads them again.
- `tables` puts rows from the cell into the call as `src.<name>`: booleans, integers, numbers and strings keep their type, a column of anything else becomes JSON.
- Values come back JSON-safe in both languages: integers as JSON numbers or, when large, strings; nested values as JSON.

See `xd://session_query/sql` for the relations, what persists between calls, and what SQL may do.
