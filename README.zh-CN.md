# omp-sessions

[English](README.md) | 简体中文

- **`session://` scheme（对 `~/.omp/agent/sessions` 的 read、grep、glob）**
  - scheme 注册 — 功能：在 omp 进程级内部 URL 路由里注册只读的 `session` scheme，主会话、子代理和轻量模式都能用；不替换 omp 自有的 scheme 或其他所有者的处理器。时机：扩展加载时一次。实例：冲突时 omp 日志写 `omp-sessions: <reason>`。
  - `session://README` — 功能：返回完整的 URL 语法。时机：模型读取时。实例：`read session://README`。
  - 查找 — 功能：匹配提示历史（omp 在 `history.db` 里的 FTS 索引）、标题和 recap，并以 JSONL 列出会话元数据。时机：读取该 URL 时。实例：`session://find?text=release workflow&project=.&max=5`、`session://here/sessions.jsonl`、`session://projects/<bucket>/sessions.jsonl`。
  - 时间线 — 功能：渲染会话的活动分支：user、developer、thinking、助手文本、IRC、任务派发和子代理结果完整显示，其他工具调用各占一行 `tool(args) status #<result entry> // intent`。时机：读取该 URL 时。实例：`session://01a1f4a1/timeline`。
  - 条目、事件和原文件 — 功能：完整显示单个条目、以 JSONL 给出同样的事件，或返回会话文件本身。时机：读取该 URL 时。实例：`session://01a1/entry/<id>`、`session://01a1/events.jsonl`、`session://01a1/raw.jsonl`。
  - 子代理、artifact 和 local 文件 — 功能：为每个子代理重复同一棵树，并暴露该会话的 `artifact://` 和 `local://`。时机：读取该 URL 时。实例：`session://01a1/agents/ReadmePeers/timeline`、`session://01a1/artifacts/4`、`session://01a1/local/readme-contract.md`。
  - 过滤与 jq — 功能：按事件类型、分组、`call:<tool>`、`marker:<what>` 或排除项以及时间窗口过滤 `timeline` 和 `events.jsonl`，并用 `?q=` 对 `events.jsonl`、`sessions.jsonl` 和 `find` 运行 jq。时机：URL 带查询参数时。实例：`session://01a1/timeline?type=thinking,irc&since=2h`、`session://sessions.jsonl?q=<jq>`。
  - grep 树 — 功能：给 `grep`/`glob` 提供一棵可遍历的树，当前项目别名为 `session://here/…`，`session://type/<filter>/<path>` 是经得起 glob 的过滤写法。时机：`grep` 或 `glob` 遍历 `session://` 路径时。实例：对 `session://here/2026-11-20/*/timeline` 执行 `grep`；`session://projects/<bucket>/<YYYY-MM-DD>/<full-id>/agents/<AgentId>/timeline`。
  - 按需扫描 — 功能：用投影式分词器和一个复用的读缓冲读取文件，先用索引遍历选出活动分支，再只读取要显示的条目；不做缓存，jq 使用的临时 JSONL 在读取返回前删除。时机：每次读取 `session://`。实例：`src/scanner.ts`。
- **`session_query` 工具（用 JS/Python 和 SQL 查同一份历史）**
  - 工具注册与启用 — 功能：以未启用状态注册 `session_query`，再经 `applyActiveTools` 按规范顺序启用（内置工具按注册表顺序在前，其余按名称），使基础 system prompt 在 `/restart` 前后不变。时机：加载时注册；`session_start` 时启用。实例：eval 单元里 `(await tool.session_query({ action: "sessions", text: "release" })).details`。
  - `sessions` / `agents` / `events` action — 功能：返回会话元数据行（按项目、关键词和 `modified` 时间窗口过滤）、一个会话的对话树，或一份对话活动分支事件的一页。时机：eval 单元调用该 action 时。实例：`tool.session_query({ action: "agents", sid: "01a1" })` → 行 `sid agent parent file bytes modified`。
  - `sql` action 与 schema `src` — 功能：运行 DuckDB SQL，解析树点名的关系（`src.entries`、`src.events`、`src.transcripts`、`src.sessions`，以及经 `tables` 传入的行）在调用时从会话文件构建，调用结束即删除。时机：每次 `sql` 调用。实例：`tool.session_query({ action: "sql", from: { sids: ["01a1"] }, query: "select count(*) from src.events" })`。
  - 每会话数据库 — 功能：在 schema `main` 里创建的表、视图和宏保存在该会话的内存数据库中（内存最多 4 GB，其余溢写到临时目录）。时机：跨调用保留；`session_switch`（`/new`、恢复、fork）和 `session_shutdown` 时关闭。实例：`CREATE TABLE main.slow AS SELECT …` 供下一次调用复用。
  - SQL 限制 — 功能：拒绝 COPY、EXPORT、ATTACH、INSTALL、LOAD、SET、PRAGMA、CALL 及其他引擎或文件语句；SQL 只读取会话目录以下的文件，不写文件。时机：每条 `sql` 语句运行前。实例：`ATTACH 'x.db'` 被拒。
  - `export` action — 功能：把一个查询结果写成调用方指定的 parquet、csv 或 json 文件，永不写到会话目录以下。时机：`export` 调用时，需要写审批（其他 action 都是 `read`）。实例：`tool.session_query({ action: "export", query: "select …", path: "/tmp/turns.parquet", format: "parquet" })`。
  - 直接调用的输出上限 — 功能：直接调用时，`details` 不超过 8192 字符才以 JSON 文本显示。时机：在 eval 之外调用 `session_query` 时。实例：`sql: more than 8192 chars of JSON in details; call from eval and read .details`。
  - `xd://session_query/actions` 与 `xd://session_query/sql` — 功能：说明各 action 返回的行，以及 `src` 关系、持久化和允许的 SQL。时机：模型读取时；工具说明最后几行指向它们。实例：``A view over `src` stops working after the call; keep results with `CREATE TABLE … AS`.``
- **宿主耦合与界面语言**
  - 宿主适配层 — 功能：所有 omp 内部 import 集中在一个文件里，宿主升级移动了某个导出时在加载时就失败。时机：扩展加载时。实例：`src/omp-adapter.ts`。
  - 工具标签 — 功能：标签显示为英文或中文；说明、topic 和结果保持英文。时机：注册工具时。实例：`Session Query` / `会话查询`，依次按 `OMOMP_LANG`、`LC_ALL`/`LC_MESSAGES`/`LANG` 决定。
