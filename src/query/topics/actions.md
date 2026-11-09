# session_query actions

Call from an eval cell and read `.details`: `(await tool.session_query({ action: "sql", … })).details` in JavaScript, `(await tool.session_query({...}))["details"]` in Python. Every action reads the session files as they are when it runs. A direct call shows `details` as JSON text when it fits in 8192 characters, otherwise only `<action>: more than 8192 chars of JSON in details; call from eval and read .details`.

| action | details |
|---|---|
| `sessions` | `{rows}`: sid url timeline project cwd title created modified status assistantTurns size recap parent, newest first; every filter applies before `limit` |
| `agents` | `{rows}`: sid agent parent file bytes modified; the root has agent "" and parent null; a subagent's parent is the allocated id it ran under |
| `events` | `{rows, total, offset, next}`: the records of `session://<id>/events.jsonl`; `next` is the next offset or null |
| `sql` | `{kind: "rows", columns, rows, truncated}` or `{kind: "changes", changes}` |
| `export` | `{path, format, rows, columns}` |

- Values come back JSON-safe in both languages: integers as JSON numbers or, when large, strings; nested values as JSON.

See `xd://session_query/sql` for the relations, what persists between calls, and what SQL may do.
