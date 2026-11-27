# omp-sessions

[English](README.md) | 简体中文

- **`session://` scheme（对 `~/.omp/agent/sessions` 的 read、grep、glob）**
  - scheme 注册 — 功能：在 omp 进程级内部 URL 路由里注册只读的 `session` scheme，主会话、子代理和轻量模式都能用；不替换 omp 自有的 scheme 或其他所有者的处理器。时机：扩展加载时一次。实例：冲突时 omp 日志写 `omp-sessions: <reason>`。
  - URL 语法 — 功能：完整的 `session://` 语法收在 omp 自己的 URL 说明里，系统提示词中 `session://` 那一行指向它。时机：模型读取时。实例：`read help://read/urls`。
  - 找会话 — 功能：列项目和会话、搜索 prompt、取 recap 都用 omp 的 `archive` eval 全局；它给出的会话 id 可以直接用在任何 `session://` URL 里，也可以作为 `session_query` 的 `sid`。`session://find` 与各个 `sessions.jsonl` 路由已删除，prompt 搜索、标题和 recap 只在 `archive` 里。时机：在 eval 单元里。实例：`archive.sessions({ project: "*", limit: 5 })`、`archive.recaps({ project: "*" })`。
  - 时间线 — 功能：渲染会话的活动分支：user、developer、thinking、助手文本、IRC、任务派发和子代理结果完整显示，其他工具调用各占一行 `tool(args) status #<result entry> // intent`。时机：读取该 URL 时。实例：`session://01a1f4a1/timeline`。
  - 条目、事件和原文件 — 功能：完整显示单个条目、以 JSONL 给出事件、返回会话文件本身，或返回对话树与分页事件的结构化 JSON。时机：读取该 URL 时。实例：`session://01a1/entry/<id>`、`session://01a1/events.jsonl`、`session://01a1/raw.jsonl`、`session://01a1/agents`、`session://here/agents`、`session://01a1/events?type=thinking&limit=100&offset=0`、`session://here/events`。
  - 子代理、artifact 和 local 文件 — 功能：为每个子代理重复同一棵树，并暴露该会话的 `artifact://` 和 `local://`。时机：读取该 URL 时。实例：`session://01a1/agents/ReadmePeers/timeline`、`session://01a1/artifacts/4`、`session://01a1/local/readme-contract.md`。
  - 过滤与 jq — 功能：按事件类型、分组、`call:<tool>`、`marker:<what>` 或排除项过滤 `timeline`、`events.jsonl` 和 `/events`（`timeline` 与 `events.jsonl` 还支持时间窗口及 `?q=` jq 查询）。时机：URL 带查询参数时。实例：`session://01a1/timeline?type=thinking,irc&since=2h`、`session://01a1/events.jsonl?q=<jq>`、`session://here/events?type=call:bash&limit=50`。
  - grep 树 — 功能：给 `grep`/`glob` 提供一棵可遍历的树，当前项目别名为 `session://here/…`，`session://type/<filter>/<path>` 是经得起 glob 的过滤写法。时机：`grep` 或 `glob` 遍历 `session://` 路径时。实例：对 `session://here/2026-11-20/*/timeline` 执行 `grep`；`session://projects/<bucket>/<YYYY-MM-DD>/<full-id>/agents/<AgentId>/timeline`。
  - 读进程 — 功能：每次读取启动一个 Rust 引擎 `omp-sessions-engine` 进程，由它解析 URL 用到的会话文件、返回渲染好的文本后退出；omp 里只留下这段文本。时机：每次 `session://` 的 read、`grep`、`glob`，以及 `session_query` 物化 `tables` 时。实例：每个请求在 omp 日志里留下一行 `omp-sessions engine`，带 `op`、`ms`、`transcripts`、`bytes`、`cache` 与 `peakRssBytes`。
- **`session_query` 工具（用 DuckDB SQL 查询和导出会话历史）**
  - 工具注册与启用 — 功能：以未启用状态注册 `session_query`，再经 `applyActiveTools` 按规范顺序启用（内置工具按注册表顺序在前，其余按名称），使基础 system prompt 在 `/restart` 前后不变。时机：加载时注册；`session_start` 时启用。实例：eval 单元里 `(await tool.session_query({ sids: ["01a1"], query: "select count(*) from src.events" })).details`。
  - DuckDB SQL 与 schema `src` — 功能：运行 DuckDB SQL，解析树点名的关系（`src.files`、`src.entries`、`src.events`、`src.transcripts`、`src.sessions`，以及通过 `tables` 传入的命名事件表）在调用时按 `from`（`"here"` / `"all"`）或 `sids` / `agents` 构建，调用结束即删除。`agent_scope: "root_only"` 排除子代理，否则包含全部 transcript。时机：每次 `session_query` 调用。实例：`tool.session_query({ sids: ["01a1"], query: "select count(*) from src.events" })`、`tool.session_query({ from: "here", agent_scope: "root_only", query: "select sid, title from src.sessions" })`。
  - 查询进程 — 功能：每个 omp 会话的 SQL 在一个引擎进程里运行；schema `main` 里建的表、视图和宏只存在于这个进程（内存最多 4 GB，超出部分溢写到进程结束时删除的私有临时目录），占用的是查询进程的内存而不是 omp 的。时机：会话第一次 `session_query` 调用时启动，之后的调用复用；`session_switch`（`/new`、恢复、fork）、`session_shutdown` 或有调用来自另一个 omp 会话时结束。实例：`CREATE TABLE main.slow AS SELECT …` 供下一次调用复用。
  - 查询进程退出 — 功能：查询进程意外退出（被杀或崩溃）时，进行中的调用失败，没有进行中的调用就由下一次调用失败，错误是 `engine_exited`，并写明 `main` 中的表、视图和宏已丢失；再下一次调用启动新进程，`main` 为空。时机：进程已不在之后。实例：`session_query sql failed: engine_exited: the query process exited (signal SIGKILL); the tables, views and macros in main are lost`。
  - SQL 限制 — 功能：拒绝 COPY、EXPORT、ATTACH、INSTALL、LOAD、SET、PRAGMA、CALL 及其他引擎或文件语句；SQL 只读取会话目录以下的文件，不写文件。时机：每条 SQL 语句运行前。实例：`ATTACH 'x.db'` 被拒。
  - 通过 `export_to` 导出 — 功能：把一个查询结果写成调用方指定的 parquet、csv 或 json 文件，永不写到会话目录以下。时机：传入 `export_to` 时触发 `write` 审批（不传时为 `read`）。实例：`tool.session_query({ sids: ["01a1"], query: "select …", export_to: { path: "/tmp/turns.parquet", format: "parquet" } })`。
  - 直接调用的输出上限 — 功能：直接调用时，`details` 不超过 8192 字符才以 JSON 文本显示。时机：在 eval 之外调用 `session_query` 时。实例：`sql: more than 8192 chars of JSON in details; call from eval and read .details`。
  - `help://session_query/sql` — 功能：说明 `src` 关系、`tables`、`params`、持久化、`export_to` 和允许的 SQL，包括以带标签结构绑定 SQL NULL 与空字符串。时机：模型读取时；工具说明指向它。实例：`tool.session_query({ query: "SELECT $missing IS NULL, $note = ''", params: { bindings: [{ key: "note", kind: "empty_text" }, { key: "missing", kind: "null" }] } })`。
- **引擎二进制与冷缓存**
  - 引擎二进制 — 功能：运行插件包内的 `engine/bin/omp-sessions-engine`。包里没有时，第一次调用从插件当前版本的 GitHub Release（`v<version>`）下载本平台的资产，按同一 Release 的 `digests.json` 校验 sha256 后才使用，之后的调用直接用下载好的文件。支持的平台：`darwin-arm64`、`darwin-x64`、`linux-x64`、`linux-arm64`、`win32-x64`、`win32-arm64`。时机：安装或更新后第一次 `session://` 读取或 `session_query` 调用时。实例：`https://github.com/oh-my-omp/omp-sessions/releases/download/v<version>/omp-sessions-engine-darwin-arm64`。
  - `engine_unavailable` — 功能：拿不到引擎二进制时，调用以原因失败，下一次调用会重新尝试；不会留下不完整的文件。`unsupported_platform`：omp 运行在上述六个平台之外，omp-sessions 在那里不能用。`not_built`：这个版本的 Release 还没有引擎资产（资产在发布之后上传，`digests.json` 最后一个），等 Release 列出它们后重试。`download_failed`：连不上 GitHub 或它返回了错误，能连上时重试。`digest_mismatch`：下载的资产或 `digests.json` 对不上，重试；一直如此就报告问题。时机：调用需要引擎进程却启动不了时。实例：`session://: engine_unavailable: not_built: …`、`session_query sql failed: engine_unavailable: download_failed: …`。
  - 冷缓存 — 功能：最后一条消息早于 24 小时的 transcript，引擎把解析结果存下来，之后读取或查询它时不再解析；会话文件有任何变化，对应的条目就不再使用。整个目录随时可以删，删了只会让接下来的读取变慢。时机：第一次读取或查询这样的 transcript 时。实例：omp 的 agent 缓存根下的 `omp-sessions`，默认是 `~/.omp/agent/cache/omp-sessions`。
- **界面语言**
  - 工具标签 — 功能：标签显示为英文或中文；说明、topic 和结果保持英文。时机：注册工具时。实例：`Session Query` / `会话查询`，依次按 `OMOMP_LANG`、`LC_ALL`/`LC_MESSAGES`/`LANG` 决定。
